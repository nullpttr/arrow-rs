// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Defines kernel to extract a substring of an Array
//! Supported array types: \[Large\]StringArray, \[Large\]BinaryArray

use crate::array::DictionaryArray;
use crate::buffer::MutableBuffer;
use crate::datatypes::*;
use crate::{array::*, buffer::Buffer};
use crate::{
    datatypes::DataType,
    error::{ArrowError, Result},
};
use std::cmp::Ordering;
use std::sync::Arc;

/// Returns an ArrayRef with substrings of all the elements in `array`.
///
/// # Arguments
///
/// * `start` - The start index of all substrings.
/// If `start >= 0`, then count from the start of the string,
/// otherwise count from the end of the string.
///
/// * `length`(option) - The length of all substrings.
/// If `length` is `None`, then the substring is from `start` to the end of the string.
///
/// Attention: Both `start` and `length` are counted by byte, not by char.
///
/// # Basic usage
/// ```
/// # use arrow::array::StringArray;
/// # use arrow::compute::kernels::substring::substring;
/// let array = StringArray::from(vec![Some("arrow"), None, Some("rust")]);
/// let result = substring(&array, 1, Some(4)).unwrap();
/// let result = result.as_any().downcast_ref::<StringArray>().unwrap();
/// assert_eq!(result, &StringArray::from(vec![Some("rrow"), None, Some("ust")]));
/// ```
///
/// # Error
/// - The function errors when the passed array is not a \[Large\]String array, \[Large\]Binary
///   array, or DictionaryArray with \[Large\]String or \[Large\]Binary as its value type.
/// - The function errors if the offset of a substring in the input array is at invalid char boundary (only for \[Large\]String array).
///
/// ## Example of trying to get an invalid utf-8 format substring
/// ```
/// # use arrow::array::StringArray;
/// # use arrow::compute::kernels::substring::substring;
/// let array = StringArray::from(vec![Some("E=mc²")]);
/// let error = substring(&array, 0, Some(5)).unwrap_err().to_string();
/// assert!(error.contains("invalid utf-8 boundary"));
/// ```
pub fn substring(array: &dyn Array, start: i64, length: Option<u64>) -> Result<ArrayRef> {
    macro_rules! substring_dict {
        ($kt: ident, $($t: ident: $gt: ident), *) => {
            match $kt.as_ref() {
                $(
                    &DataType::$t => {
                        let dict = array
                            .as_any()
                            .downcast_ref::<DictionaryArray<$gt>>()
                            .unwrap_or_else(|| {
                                panic!("Expect 'DictionaryArray<{}>' but got array of data type {:?}",
                                       stringify!($gt), array.data_type())
                            });
                        let values = substring(dict.values(), start, length)?;
                        let result = DictionaryArray::try_new(dict.keys(), &values)?;
                        Ok(Arc::new(result))
                    },
                )*
                    t => panic!("Unsupported dictionary key type: {}", t)
            }
        }
    }

    match array.data_type() {
        DataType::Dictionary(kt, _) => {
            substring_dict!(
                kt,
                Int8: Int8Type,
                Int16: Int16Type,
                Int32: Int32Type,
                Int64: Int64Type,
                UInt8: UInt8Type,
                UInt16: UInt16Type,
                UInt32: UInt32Type,
                UInt64: UInt64Type
            )
        }
        DataType::LargeBinary => binary_substring(
            array
                .as_any()
                .downcast_ref::<LargeBinaryArray>()
                .expect("A large binary is expected"),
            start,
            length.map(|e| e as i64),
        ),
        DataType::Binary => binary_substring(
            array
                .as_any()
                .downcast_ref::<BinaryArray>()
                .expect("A binary is expected"),
            start as i32,
            length.map(|e| e as i32),
        ),
        DataType::FixedSizeBinary(old_len) => fixed_size_binary_substring(
            array
                .as_any()
                .downcast_ref::<FixedSizeBinaryArray>()
                .expect("a fixed size binary is expected"),
            *old_len,
            start as i32,
            length.map(|e| e as i32),
        ),
        DataType::LargeUtf8 => utf8_substring(
            array
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .expect("A large string is expected"),
            start,
            length.map(|e| e as i64),
        ),
        DataType::Utf8 => utf8_substring(
            array
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("A string is expected"),
            start as i32,
            length.map(|e| e as i32),
        ),
        _ => Err(ArrowError::ComputeError(format!(
            "substring does not support type {:?}",
            array.data_type()
        ))),
    }
}

fn binary_substring<OffsetSize: OffsetSizeTrait>(
    array: &GenericBinaryArray<OffsetSize>,
    start: OffsetSize,
    length: Option<OffsetSize>,
) -> Result<ArrayRef> {
    let offsets = array.value_offsets();
    let null_bit_buffer = array.data_ref().null_buffer().cloned();
    let values = array.value_data();
    let data = values.as_slice();
    let zero = OffsetSize::zero();

    // start and end offsets of all substrings
    let mut new_starts_ends: Vec<(OffsetSize, OffsetSize)> =
        Vec::with_capacity(array.len());
    let mut new_offsets: Vec<OffsetSize> = Vec::with_capacity(array.len() + 1);
    let mut len_so_far = zero;
    new_offsets.push(zero);

    offsets.windows(2).for_each(|pair| {
        let new_start = match start.cmp(&zero) {
            Ordering::Greater => (pair[0] + start).min(pair[1]),
            Ordering::Equal => pair[0],
            Ordering::Less => (pair[1] + start).max(pair[0]),
        };
        let new_end = match length {
            Some(length) => (length + new_start).min(pair[1]),
            None => pair[1],
        };
        len_so_far += new_end - new_start;
        new_starts_ends.push((new_start, new_end));
        new_offsets.push(len_so_far);
    });

    // concatenate substrings into a buffer
    let mut new_values =
        MutableBuffer::new(new_offsets.last().unwrap().to_usize().unwrap());

    new_starts_ends
        .iter()
        .map(|(start, end)| {
            let start = start.to_usize().unwrap();
            let end = end.to_usize().unwrap();
            &data[start..end]
        })
        .for_each(|slice| new_values.extend_from_slice(slice));

    let data = unsafe {
        ArrayData::new_unchecked(
            GenericBinaryArray::<OffsetSize>::get_data_type(),
            array.len(),
            None,
            null_bit_buffer,
            0,
            vec![Buffer::from_slice_ref(&new_offsets), new_values.into()],
            vec![],
        )
    };
    Ok(make_array(data))
}

fn fixed_size_binary_substring(
    array: &FixedSizeBinaryArray,
    old_len: i32,
    start: i32,
    length: Option<i32>,
) -> Result<ArrayRef> {
    let new_start = if start >= 0 {
        start.min(old_len)
    } else {
        (old_len + start).max(0)
    };
    let new_len = match length {
        Some(len) => len.min(old_len - new_start),
        None => old_len - new_start,
    };

    // build value buffer
    let num_of_elements = array.len();
    let values = array.value_data();
    let data = values.as_slice();
    let mut new_values = MutableBuffer::new(num_of_elements * (new_len as usize));
    (0..num_of_elements)
        .map(|idx| {
            let offset = array.value_offset(idx);
            (
                (offset + new_start) as usize,
                (offset + new_start + new_len) as usize,
            )
        })
        .for_each(|(start, end)| new_values.extend_from_slice(&data[start..end]));

    let array_data = unsafe {
        ArrayData::new_unchecked(
            DataType::FixedSizeBinary(new_len),
            num_of_elements,
            None,
            array
                .data_ref()
                .null_buffer()
                .map(|b| b.bit_slice(array.offset(), num_of_elements)),
            0,
            vec![new_values.into()],
            vec![],
        )
    };

    Ok(make_array(array_data))
}

/// substring by byte
fn utf8_substring<OffsetSize: OffsetSizeTrait>(
    array: &GenericStringArray<OffsetSize>,
    start: OffsetSize,
    length: Option<OffsetSize>,
) -> Result<ArrayRef> {
    let offsets = array.value_offsets();
    let null_bit_buffer = array.data_ref().null_buffer().cloned();
    let values = array.value_data();
    let data = values.as_slice();
    let zero = OffsetSize::zero();

    // Check if `offset` is at a valid char boundary.
    // If yes, return `offset`, else return error
    let check_char_boundary = {
        // Safety: a StringArray must contain valid UTF8 data
        let data_str = unsafe { std::str::from_utf8_unchecked(data) };
        |offset: OffsetSize| {
            let offset_usize = offset.to_usize().unwrap();
            if data_str.is_char_boundary(offset_usize) {
                Ok(offset)
            } else {
                Err(ArrowError::ComputeError(format!(
                    "The offset {} is at an invalid utf-8 boundary.",
                    offset_usize
                )))
            }
        }
    };

    // start and end offsets of all substrings
    let mut new_starts_ends: Vec<(OffsetSize, OffsetSize)> =
        Vec::with_capacity(array.len());
    let mut new_offsets: Vec<OffsetSize> = Vec::with_capacity(array.len() + 1);
    let mut len_so_far = zero;
    new_offsets.push(zero);

    offsets.windows(2).try_for_each(|pair| -> Result<()> {
        let new_start = match start.cmp(&zero) {
            Ordering::Greater => check_char_boundary((pair[0] + start).min(pair[1]))?,
            Ordering::Equal => pair[0],
            Ordering::Less => check_char_boundary((pair[1] + start).max(pair[0]))?,
        };
        let new_end = match length {
            Some(length) => check_char_boundary((length + new_start).min(pair[1]))?,
            None => pair[1],
        };
        len_so_far += new_end - new_start;
        new_starts_ends.push((new_start, new_end));
        new_offsets.push(len_so_far);
        Ok(())
    })?;

    // concatenate substrings into a buffer
    let mut new_values =
        MutableBuffer::new(new_offsets.last().unwrap().to_usize().unwrap());

    new_starts_ends
        .iter()
        .map(|(start, end)| {
            let start = start.to_usize().unwrap();
            let end = end.to_usize().unwrap();
            &data[start..end]
        })
        .for_each(|slice| new_values.extend_from_slice(slice));

    let data = unsafe {
        ArrayData::new_unchecked(
            GenericStringArray::<OffsetSize>::get_data_type(),
            array.len(),
            None,
            null_bit_buffer,
            0,
            vec![Buffer::from_slice_ref(&new_offsets), new_values.into()],
            vec![],
        )
    };
    Ok(make_array(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::*;

    #[allow(clippy::type_complexity)]
    fn with_nulls_generic_binary<O: OffsetSizeTrait>() -> Result<()> {
        let cases: Vec<(Vec<Option<&[u8]>>, i64, Option<u64>, Vec<Option<&[u8]>>)> = vec![
            // all-nulls array is always identical
            (vec![None, None, None], -1, Some(1), vec![None, None, None]),
            // identity
            (
                vec![Some(b"hello"), None, Some(&[0xf8, 0xf9, 0xff, 0xfa])],
                0,
                None,
                vec![Some(b"hello"), None, Some(&[0xf8, 0xf9, 0xff, 0xfa])],
            ),
            // 0 length -> Nothing
            (
                vec![Some(b"hello"), None, Some(&[0xf8, 0xf9, 0xff, 0xfa])],
                0,
                Some(0),
                vec![Some(&[]), None, Some(&[])],
            ),
            // high start -> Nothing
            (
                vec![Some(b"hello"), None, Some(&[0xf8, 0xf9, 0xff, 0xfa])],
                1000,
                Some(0),
                vec![Some(&[]), None, Some(&[])],
            ),
            // high negative start -> identity
            (
                vec![Some(b"hello"), None, Some(&[0xf8, 0xf9, 0xff, 0xfa])],
                -1000,
                None,
                vec![Some(b"hello"), None, Some(&[0xf8, 0xf9, 0xff, 0xfa])],
            ),
            // high length -> identity
            (
                vec![Some(b"hello"), None, Some(&[0xf8, 0xf9, 0xff, 0xfa])],
                0,
                Some(1000),
                vec![Some(b"hello"), None, Some(&[0xf8, 0xf9, 0xff, 0xfa])],
            ),
        ];

        cases.into_iter().try_for_each::<_, Result<()>>(
            |(array, start, length, expected)| {
                let array = GenericBinaryArray::<O>::from(array);
                let result: ArrayRef = substring(&array, start, length)?;
                assert_eq!(array.len(), result.len());

                let result = result
                    .as_any()
                    .downcast_ref::<GenericBinaryArray<O>>()
                    .unwrap();
                let expected = GenericBinaryArray::<O>::from(expected);
                assert_eq!(&expected, result);
                Ok(())
            },
        )?;

        Ok(())
    }

    #[test]
    fn with_nulls_binary() -> Result<()> {
        with_nulls_generic_binary::<i32>()
    }

    #[test]
    fn with_nulls_large_binary() -> Result<()> {
        with_nulls_generic_binary::<i64>()
    }

    #[allow(clippy::type_complexity)]
    fn without_nulls_generic_binary<O: OffsetSizeTrait>() -> Result<()> {
        let cases: Vec<(Vec<&[u8]>, i64, Option<u64>, Vec<&[u8]>)> = vec![
            // empty array is always identical
            (vec![b"", b"", b""], 2, Some(1), vec![b"", b"", b""]),
            // increase start
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                0,
                None,
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                1,
                None,
                vec![b"ello", b"", &[0xf9, 0xff, 0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                2,
                None,
                vec![b"llo", b"", &[0xff, 0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                3,
                None,
                vec![b"lo", b"", &[0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                10,
                None,
                vec![b"", b"", b""],
            ),
            // increase start negatively
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                -1,
                None,
                vec![b"o", b"", &[0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                -2,
                None,
                vec![b"lo", b"", &[0xff, 0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                -3,
                None,
                vec![b"llo", b"", &[0xf9, 0xff, 0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                -10,
                None,
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
            ),
            // increase length
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                1,
                Some(1),
                vec![b"e", b"", &[0xf9]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                1,
                Some(2),
                vec![b"el", b"", &[0xf9, 0xff]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                1,
                Some(3),
                vec![b"ell", b"", &[0xf9, 0xff, 0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                1,
                Some(4),
                vec![b"ello", b"", &[0xf9, 0xff, 0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                -3,
                Some(1),
                vec![b"l", b"", &[0xf9]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                -3,
                Some(2),
                vec![b"ll", b"", &[0xf9, 0xff]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                -3,
                Some(3),
                vec![b"llo", b"", &[0xf9, 0xff, 0xfa]],
            ),
            (
                vec![b"hello", b"", &[0xf8, 0xf9, 0xff, 0xfa]],
                -3,
                Some(4),
                vec![b"llo", b"", &[0xf9, 0xff, 0xfa]],
            ),
        ];

        cases.into_iter().try_for_each::<_, Result<()>>(
            |(array, start, length, expected)| {
                let array = GenericBinaryArray::<O>::from(array);
                let result = substring(&array, start, length)?;
                assert_eq!(array.len(), result.len());
                let result = result
                    .as_any()
                    .downcast_ref::<GenericBinaryArray<O>>()
                    .unwrap();
                let expected = GenericBinaryArray::<O>::from(expected);
                assert_eq!(&expected, result,);
                Ok(())
            },
        )?;

        Ok(())
    }

    #[test]
    fn without_nulls_binary() -> Result<()> {
        without_nulls_generic_binary::<i32>()
    }

    #[test]
    fn without_nulls_large_binary() -> Result<()> {
        without_nulls_generic_binary::<i64>()
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn with_nulls_fixed_size_binary() -> Result<()> {
        let cases: Vec<(Vec<Option<&[u8]>>, i64, Option<u64>, Vec<Option<&[u8]>>)> = vec![
            // all-nulls array is always identical
            (vec![None, None, None], 3, Some(2), vec![None, None, None]),
            // increase start
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                0,
                None,
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                1,
                None,
                vec![Some(b"at"), None, Some(&[0xf9, 0xff])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                2,
                None,
                vec![Some(b"t"), None, Some(&[0xff])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                3,
                None,
                vec![Some(b""), None, Some(&[])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                10,
                None,
                vec![Some(b""), None, Some(b"")],
            ),
            // increase start negatively
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                -1,
                None,
                vec![Some(b"t"), None, Some(&[0xff])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                -2,
                None,
                vec![Some(b"at"), None, Some(&[0xf9, 0xff])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                -3,
                None,
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                -10,
                None,
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
            ),
            // increase length
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                1,
                Some(1),
                vec![Some(b"a"), None, Some(&[0xf9])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                1,
                Some(2),
                vec![Some(b"at"), None, Some(&[0xf9, 0xff])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                1,
                Some(3),
                vec![Some(b"at"), None, Some(&[0xf9, 0xff])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                -3,
                Some(1),
                vec![Some(b"c"), None, Some(&[0xf8])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                -3,
                Some(2),
                vec![Some(b"ca"), None, Some(&[0xf8, 0xf9])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                -3,
                Some(3),
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
            ),
            (
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
                -3,
                Some(4),
                vec![Some(b"cat"), None, Some(&[0xf8, 0xf9, 0xff])],
            ),
        ];

        cases.into_iter().try_for_each::<_, Result<()>>(
            |(array, start, length, expected)| {
                let array = FixedSizeBinaryArray::try_from_sparse_iter(array.into_iter())
                    .unwrap();
                let result = substring(&array, start, length)?;
                assert_eq!(array.len(), result.len());
                let result = result
                    .as_any()
                    .downcast_ref::<FixedSizeBinaryArray>()
                    .unwrap();
                let expected =
                    FixedSizeBinaryArray::try_from_sparse_iter(expected.into_iter())
                        .unwrap();
                assert_eq!(&expected, result,);
                Ok(())
            },
        )?;

        Ok(())
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn without_nulls_fixed_size_binary() -> Result<()> {
        let cases: Vec<(Vec<&[u8]>, i64, Option<u64>, Vec<&[u8]>)> = vec![
            // empty array is always identical
            (vec![b"", b"", &[]], 3, Some(2), vec![b"", b"", &[]]),
            // increase start
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                0,
                None,
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                1,
                None,
                vec![b"at", b"og", &[0xf9, 0xff]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                2,
                None,
                vec![b"t", b"g", &[0xff]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                3,
                None,
                vec![b"", b"", &[]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                10,
                None,
                vec![b"", b"", b""],
            ),
            // increase start negatively
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                -1,
                None,
                vec![b"t", b"g", &[0xff]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                -2,
                None,
                vec![b"at", b"og", &[0xf9, 0xff]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                -3,
                None,
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                -10,
                None,
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
            ),
            // increase length
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                1,
                Some(1),
                vec![b"a", b"o", &[0xf9]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                1,
                Some(2),
                vec![b"at", b"og", &[0xf9, 0xff]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                1,
                Some(3),
                vec![b"at", b"og", &[0xf9, 0xff]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                -3,
                Some(1),
                vec![b"c", b"d", &[0xf8]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                -3,
                Some(2),
                vec![b"ca", b"do", &[0xf8, 0xf9]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                -3,
                Some(3),
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
            ),
            (
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
                -3,
                Some(4),
                vec![b"cat", b"dog", &[0xf8, 0xf9, 0xff]],
            ),
        ];

        cases.into_iter().try_for_each::<_, Result<()>>(
            |(array, start, length, expected)| {
                let array =
                    FixedSizeBinaryArray::try_from_iter(array.into_iter()).unwrap();
                let result = substring(&array, start, length)?;
                assert_eq!(array.len(), result.len());
                let result = result
                    .as_any()
                    .downcast_ref::<FixedSizeBinaryArray>()
                    .unwrap();
                let expected =
                    FixedSizeBinaryArray::try_from_iter(expected.into_iter()).unwrap();
                assert_eq!(&expected, result,);
                Ok(())
            },
        )?;

        Ok(())
    }

    #[test]
    fn offset_fixed_size_binary() -> Result<()> {
        let values: [u8; 15] = *b"hellotherearrow";
        // set the first and third element to be valid
        let bits_v = [0b101_u8];

        let data = ArrayData::builder(DataType::FixedSizeBinary(5))
            .len(2)
            .add_buffer(Buffer::from(&values[..]))
            .offset(1)
            .null_bit_buffer(Buffer::from(bits_v))
            .build()
            .unwrap();
        // array is `[null, "arrow"]`
        let array = FixedSizeBinaryArray::from(data);
        // result is `[null, "rrow"]`
        let result = substring(&array, 1, None)?;
        let result = result
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        let expected = FixedSizeBinaryArray::try_from_sparse_iter(
            vec![None, Some(b"rrow")].into_iter(),
        )
        .unwrap();
        assert_eq!(result, &expected);

        Ok(())
    }

    fn with_nulls_generic_string<O: OffsetSizeTrait>() -> Result<()> {
        let cases = vec![
            // all-nulls array is always identical
            (vec![None, None, None], 0, None, vec![None, None, None]),
            // identity
            (
                vec![Some("hello"), None, Some("word")],
                0,
                None,
                vec![Some("hello"), None, Some("word")],
            ),
            // 0 length -> Nothing
            (
                vec![Some("hello"), None, Some("word")],
                0,
                Some(0),
                vec![Some(""), None, Some("")],
            ),
            // high start -> Nothing
            (
                vec![Some("hello"), None, Some("word")],
                1000,
                Some(0),
                vec![Some(""), None, Some("")],
            ),
            // high negative start -> identity
            (
                vec![Some("hello"), None, Some("word")],
                -1000,
                None,
                vec![Some("hello"), None, Some("word")],
            ),
            // high length -> identity
            (
                vec![Some("hello"), None, Some("word")],
                0,
                Some(1000),
                vec![Some("hello"), None, Some("word")],
            ),
        ];

        cases.into_iter().try_for_each::<_, Result<()>>(
            |(array, start, length, expected)| {
                let array = GenericStringArray::<O>::from(array);
                let result: ArrayRef = substring(&array, start, length)?;
                assert_eq!(array.len(), result.len());

                let result = result
                    .as_any()
                    .downcast_ref::<GenericStringArray<O>>()
                    .unwrap();
                let expected = GenericStringArray::<O>::from(expected);
                assert_eq!(&expected, result);
                Ok(())
            },
        )?;

        Ok(())
    }

    #[test]
    fn with_nulls_string() -> Result<()> {
        with_nulls_generic_string::<i32>()
    }

    #[test]
    fn with_nulls_large_string() -> Result<()> {
        with_nulls_generic_string::<i64>()
    }

    fn without_nulls_generic_string<O: OffsetSizeTrait>() -> Result<()> {
        let cases = vec![
            // empty array is always identical
            (vec!["", "", ""], 0, None, vec!["", "", ""]),
            // increase start
            (
                vec!["hello", "", "word"],
                0,
                None,
                vec!["hello", "", "word"],
            ),
            (vec!["hello", "", "word"], 1, None, vec!["ello", "", "ord"]),
            (vec!["hello", "", "word"], 2, None, vec!["llo", "", "rd"]),
            (vec!["hello", "", "word"], 3, None, vec!["lo", "", "d"]),
            (vec!["hello", "", "word"], 10, None, vec!["", "", ""]),
            // increase start negatively
            (vec!["hello", "", "word"], -1, None, vec!["o", "", "d"]),
            (vec!["hello", "", "word"], -2, None, vec!["lo", "", "rd"]),
            (vec!["hello", "", "word"], -3, None, vec!["llo", "", "ord"]),
            (
                vec!["hello", "", "word"],
                -10,
                None,
                vec!["hello", "", "word"],
            ),
            // increase length
            (vec!["hello", "", "word"], 1, Some(1), vec!["e", "", "o"]),
            (vec!["hello", "", "word"], 1, Some(2), vec!["el", "", "or"]),
            (
                vec!["hello", "", "word"],
                1,
                Some(3),
                vec!["ell", "", "ord"],
            ),
            (
                vec!["hello", "", "word"],
                1,
                Some(4),
                vec!["ello", "", "ord"],
            ),
            (vec!["hello", "", "word"], -3, Some(1), vec!["l", "", "o"]),
            (vec!["hello", "", "word"], -3, Some(2), vec!["ll", "", "or"]),
            (
                vec!["hello", "", "word"],
                -3,
                Some(3),
                vec!["llo", "", "ord"],
            ),
            (
                vec!["hello", "", "word"],
                -3,
                Some(4),
                vec!["llo", "", "ord"],
            ),
        ];

        cases.into_iter().try_for_each::<_, Result<()>>(
            |(array, start, length, expected)| {
                let array = GenericStringArray::<O>::from(array);
                let result = substring(&array, start, length)?;
                assert_eq!(array.len(), result.len());
                let result = result
                    .as_any()
                    .downcast_ref::<GenericStringArray<O>>()
                    .unwrap();
                let expected = GenericStringArray::<O>::from(expected);
                assert_eq!(&expected, result,);
                Ok(())
            },
        )?;

        Ok(())
    }

    #[test]
    fn without_nulls_string() -> Result<()> {
        without_nulls_generic_string::<i32>()
    }

    #[test]
    fn without_nulls_large_string() -> Result<()> {
        without_nulls_generic_string::<i64>()
    }

    #[test]
    fn dictionary() -> Result<()> {
        _dictionary::<Int8Type>()?;
        _dictionary::<Int16Type>()?;
        _dictionary::<Int32Type>()?;
        _dictionary::<Int64Type>()?;
        _dictionary::<UInt8Type>()?;
        _dictionary::<UInt16Type>()?;
        _dictionary::<UInt32Type>()?;
        _dictionary::<UInt64Type>()?;
        Ok(())
    }

    fn _dictionary<K: ArrowDictionaryKeyType>() -> Result<()> {
        const TOTAL: i32 = 100;

        let v = ["aaa", "bbb", "ccc", "ddd", "eee"];
        let data: Vec<Option<&str>> = (0..TOTAL)
            .map(|n| {
                let i = n % 5;
                if i == 3 {
                    None
                } else {
                    Some(v[i as usize])
                }
            })
            .collect();

        let dict_array: DictionaryArray<K> = data.clone().into_iter().collect();

        let expected: Vec<Option<&str>> =
            data.iter().map(|opt| opt.map(|s| &s[1..3])).collect();

        let res = substring(&dict_array, 1, Some(2))?;
        let actual = res.as_any().downcast_ref::<DictionaryArray<K>>().unwrap();
        let actual: Vec<Option<&str>> = actual
            .values()
            .as_any()
            .downcast_ref::<GenericStringArray<i32>>()
            .unwrap()
            .take_iter(actual.keys_iter())
            .collect();

        for i in 0..TOTAL as usize {
            assert_eq!(expected[i], actual[i],);
        }

        Ok(())
    }

    #[test]
    fn check_invalid_array_type() {
        let array = Int32Array::from(vec![Some(1), Some(2), Some(3)]);
        let err = substring(&array, 0, None).unwrap_err().to_string();
        assert!(err.contains("substring does not support type"));
    }

    // tests for the utf-8 validation checking
    #[test]
    fn check_start_index() {
        let array = StringArray::from(vec![Some("E=mc²"), Some("ascii")]);
        let err = substring(&array, -1, None).unwrap_err().to_string();
        assert!(err.contains("invalid utf-8 boundary"));
    }

    #[test]
    fn check_length() {
        let array = StringArray::from(vec![Some("E=mc²"), Some("ascii")]);
        let err = substring(&array, 0, Some(5)).unwrap_err().to_string();
        assert!(err.contains("invalid utf-8 boundary"));
    }
}
