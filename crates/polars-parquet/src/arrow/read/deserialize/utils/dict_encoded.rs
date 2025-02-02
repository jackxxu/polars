use arrow::bitmap::bitmask::BitMask;
use arrow::bitmap::{Bitmap, MutableBitmap};
use arrow::types::{AlignedBytes, NativeType};
use polars_compute::filter::filter_boolean_kernel;

use super::filter_from_range;
use crate::parquet::encoding::hybrid_rle::{HybridRleChunk, HybridRleDecoder};
use crate::parquet::error::ParquetResult;
use crate::read::{Filter, ParquetError};

pub fn decode_dict<T: NativeType>(
    values: HybridRleDecoder<'_>,
    dict: &[T],
    is_optional: bool,
    page_validity: Option<&Bitmap>,
    filter: Option<Filter>,
    validity: &mut MutableBitmap,
    target: &mut Vec<T>,
) -> ParquetResult<()> {
    decode_dict_dispatch(
        values,
        bytemuck::cast_slice(dict),
        is_optional,
        page_validity,
        filter,
        validity,
        <T::AlignedBytes as AlignedBytes>::cast_vec_ref_mut(target),
    )
}

pub(crate) fn append_validity(
    page_validity: Option<&Bitmap>,
    filter: Option<&Filter>,
    validity: &mut MutableBitmap,
    values_len: usize,
) {
    match (page_validity, filter) {
        (None, None) => validity.extend_constant(values_len, true),
        (None, Some(f)) => validity.extend_constant(f.num_rows(), true),
        (Some(page_validity), None) => validity.extend_from_bitmap(page_validity),
        (Some(page_validity), Some(Filter::Range(rng))) => {
            let page_validity = page_validity.clone();
            validity.extend_from_bitmap(&page_validity.clone().sliced(rng.start, rng.len()))
        },
        (Some(page_validity), Some(Filter::Mask(mask))) => {
            validity.extend_from_bitmap(&filter_boolean_kernel(page_validity, mask))
        },
    }
}

pub(crate) fn constrain_page_validity(
    values_len: usize,
    page_validity: Option<&Bitmap>,
    filter: Option<&Filter>,
) -> Option<Bitmap> {
    let num_unfiltered_rows = match (filter.as_ref(), page_validity) {
        (None, None) => values_len,
        (None, Some(pv)) => {
            debug_assert!(pv.len() >= values_len);
            pv.len()
        },
        (Some(f), v) => {
            if cfg!(debug_assertions) {
                if let Some(v) = v {
                    assert!(v.len() >= f.max_offset());
                }
            }

            f.max_offset()
        },
    };

    page_validity.map(|pv| {
        if pv.len() > num_unfiltered_rows {
            pv.clone().sliced(0, num_unfiltered_rows)
        } else {
            pv.clone()
        }
    })
}

#[inline(never)]
pub fn decode_dict_dispatch<B: AlignedBytes>(
    mut values: HybridRleDecoder<'_>,
    dict: &[B],
    is_optional: bool,
    page_validity: Option<&Bitmap>,
    filter: Option<Filter>,
    validity: &mut MutableBitmap,
    target: &mut Vec<B>,
) -> ParquetResult<()> {
    if cfg!(debug_assertions) && is_optional {
        assert_eq!(target.len(), validity.len());
    }

    if is_optional {
        append_validity(page_validity, filter.as_ref(), validity, values.len());
    }

    let page_validity = constrain_page_validity(values.len(), page_validity, filter.as_ref());

    match (filter, page_validity) {
        (None, None) => decode_required_dict(values, dict, target),
        (Some(Filter::Range(rng)), None) if rng.start == 0 => {
            values.limit_to(rng.end);
            decode_required_dict(values, dict, target)
        },
        (None, Some(page_validity)) => decode_optional_dict(values, dict, &page_validity, target),
        (Some(Filter::Range(rng)), Some(page_validity)) if rng.start == 0 => {
            decode_optional_dict(values, dict, &page_validity, target)
        },
        (Some(Filter::Mask(filter)), None) => {
            decode_masked_required_dict(values, dict, &filter, target)
        },
        (Some(Filter::Mask(filter)), Some(page_validity)) => {
            decode_masked_optional_dict(values, dict, &filter, &page_validity, target)
        },
        (Some(Filter::Range(rng)), None) => {
            decode_masked_required_dict(values, dict, &filter_from_range(rng.clone()), target)
        },
        (Some(Filter::Range(rng)), Some(page_validity)) => decode_masked_optional_dict(
            values,
            dict,
            &filter_from_range(rng.clone()),
            &page_validity,
            target,
        ),
    }?;

    if cfg!(debug_assertions) && is_optional {
        assert_eq!(target.len(), validity.len());
    }

    Ok(())
}

#[cold]
fn oob_dict_idx() -> ParquetError {
    ParquetError::oos("Dictionary Index is out-of-bounds")
}

#[inline(always)]
fn verify_dict_indices(indices: &[u32; 32], dict_size: usize) -> ParquetResult<()> {
    let mut is_valid = true;
    for &idx in indices {
        is_valid &= (idx as usize) < dict_size;
    }

    if is_valid {
        return Ok(());
    }

    Err(oob_dict_idx())
}

#[inline(never)]
pub fn decode_required_dict<B: AlignedBytes>(
    mut values: HybridRleDecoder<'_>,
    dict: &[B],
    target: &mut Vec<B>,
) -> ParquetResult<()> {
    if dict.is_empty() && values.len() > 0 {
        return Err(oob_dict_idx());
    }

    let start_length = target.len();
    let end_length = start_length + values.len();

    target.reserve(values.len());
    let mut target_ptr = unsafe { target.as_mut_ptr().add(start_length) };

    while values.len() > 0 {
        let chunk = values.next_chunk()?.unwrap();

        match chunk {
            HybridRleChunk::Rle(value, length) => {
                if length == 0 {
                    continue;
                }

                let target_slice;
                // SAFETY:
                // 1. `target_ptr..target_ptr + values.len()` is allocated
                // 2. `length <= limit`
                unsafe {
                    target_slice = std::slice::from_raw_parts_mut(target_ptr, length);
                    target_ptr = target_ptr.add(length);
                }

                let Some(&value) = dict.get(value as usize) else {
                    return Err(oob_dict_idx());
                };

                target_slice.fill(value);
            },
            HybridRleChunk::Bitpacked(mut decoder) => {
                let mut chunked = decoder.chunked();
                for chunk in chunked.by_ref() {
                    verify_dict_indices(&chunk, dict.len())?;

                    for (i, &idx) in chunk.iter().enumerate() {
                        unsafe { target_ptr.add(i).write(*dict.get_unchecked(idx as usize)) };
                    }
                    unsafe {
                        target_ptr = target_ptr.add(32);
                    }
                }

                if let Some((chunk, chunk_size)) = chunked.remainder() {
                    let highest_idx = chunk[..chunk_size].iter().copied().max().unwrap();
                    if highest_idx as usize >= dict.len() {
                        return Err(oob_dict_idx());
                    }

                    for (i, &idx) in chunk[..chunk_size].iter().enumerate() {
                        unsafe { target_ptr.add(i).write(*dict.get_unchecked(idx as usize)) };
                    }
                    unsafe {
                        target_ptr = target_ptr.add(chunk_size);
                    }
                }
            },
        }
    }

    unsafe {
        target.set_len(end_length);
    }

    Ok(())
}

#[inline(never)]
pub fn decode_optional_dict<B: AlignedBytes>(
    mut values: HybridRleDecoder<'_>,
    dict: &[B],
    validity: &Bitmap,
    target: &mut Vec<B>,
) -> ParquetResult<()> {
    let num_valid_values = validity.set_bits();

    // Dispatch to the required kernel if all rows are valid anyway.
    if num_valid_values == validity.len() {
        values.limit_to(validity.len());
        return decode_required_dict(values, dict, target);
    }

    if dict.is_empty() && num_valid_values > 0 {
        return Err(oob_dict_idx());
    }

    assert!(num_valid_values <= values.len());
    let start_length = target.len();
    let end_length = start_length + validity.len();

    target.reserve(validity.len());
    let mut target_ptr = unsafe { target.as_mut_ptr().add(start_length) };

    values.limit_to(num_valid_values);
    let mut validity = BitMask::from_bitmap(validity);
    let mut values_buffer = [0u32; 128];
    let values_buffer = &mut values_buffer;

    for chunk in values.into_chunk_iter() {
        match chunk? {
            HybridRleChunk::Rle(value, size) => {
                if size == 0 {
                    continue;
                }

                // If we know that we have `size` times `value` that we can append, but there might
                // be nulls in between those values.
                //
                // 1. See how many `num_rows = valid + invalid` values `size` would entail. This is
                //    done with `num_bits_before_nth_one` on the validity mask.
                // 2. Fill `num_rows` values into the target buffer.
                // 3. Advance the validity mask by `num_rows` values.

                let num_chunk_rows = validity.nth_set_bit_idx(size, 0).unwrap_or(validity.len());

                (_, validity) = unsafe { validity.split_at_unchecked(num_chunk_rows) };

                let Some(&value) = dict.get(value as usize) else {
                    return Err(oob_dict_idx());
                };

                let target_slice;
                // SAFETY:
                // Given `validity_iter` before the `advance_by_bits`
                //
                // 1. `target_ptr..target_ptr + validity_iter.bits_left()` is allocated
                // 2. `num_chunk_rows <= validity_iter.bits_left()`
                unsafe {
                    target_slice = std::slice::from_raw_parts_mut(target_ptr, num_chunk_rows);
                    target_ptr = target_ptr.add(num_chunk_rows);
                }

                target_slice.fill(value);
            },
            HybridRleChunk::Bitpacked(mut decoder) => {
                let mut chunked = decoder.chunked();

                let mut buffer_part_idx = 0;
                let mut values_offset = 0;
                let mut num_buffered: usize = 0;

                {
                    let mut num_done = 0;
                    let mut validity_iter = validity.fast_iter_u56();

                    'outer: for v in validity_iter.by_ref() {
                        while num_buffered < v.count_ones() as usize {
                            let buffer_part = <&mut [u32; 32]>::try_from(
                                &mut values_buffer[buffer_part_idx * 32..][..32],
                            )
                            .unwrap();
                            let Some(num_added) = chunked.next_into(buffer_part) else {
                                break 'outer;
                            };

                            verify_dict_indices(buffer_part, dict.len())?;

                            num_buffered += num_added;

                            buffer_part_idx += 1;
                            buffer_part_idx %= 4;
                        }

                        let mut num_read = 0;

                        for i in 0..56 {
                            let idx = values_buffer[(values_offset + num_read) % 128];

                            // SAFETY:
                            // 1. `values_buffer` starts out as only zeros, which we know is in the
                            //    dictionary following the original `dict.is_empty` check.
                            // 2. Each time we write to `values_buffer`, it is followed by a
                            //    `verify_dict_indices`.
                            let value = unsafe { dict.get_unchecked(idx as usize) };
                            let value = *value;
                            unsafe { target_ptr.add(i).write(value) };
                            num_read += ((v >> i) & 1) as usize;
                        }

                        values_offset += num_read;
                        values_offset %= 128;
                        num_buffered -= num_read;
                        unsafe {
                            target_ptr = target_ptr.add(56);
                        }
                        num_done += 56;
                    }

                    (_, validity) = unsafe { validity.split_at_unchecked(num_done) };
                }

                let num_decoder_remaining = num_buffered + chunked.decoder.len();
                let decoder_limit = validity
                    .nth_set_bit_idx(num_decoder_remaining, 0)
                    .unwrap_or(validity.len());

                let current_validity;
                (current_validity, validity) =
                    unsafe { validity.split_at_unchecked(decoder_limit) };
                let (v, _) = current_validity.fast_iter_u56().remainder();

                while num_buffered < v.count_ones() as usize {
                    let buffer_part = <&mut [u32; 32]>::try_from(
                        &mut values_buffer[buffer_part_idx * 32..][..32],
                    )
                    .unwrap();
                    let num_added = chunked.next_into(buffer_part).unwrap();

                    verify_dict_indices(buffer_part, dict.len())?;

                    num_buffered += num_added;

                    buffer_part_idx += 1;
                    buffer_part_idx %= 4;
                }

                let mut num_read = 0;

                for i in 0..decoder_limit {
                    let idx = values_buffer[(values_offset + num_read) % 128];
                    let value = unsafe { dict.get_unchecked(idx as usize) };
                    let value = *value;
                    unsafe { *target_ptr.add(i) = value };
                    num_read += ((v >> i) & 1) as usize;
                }

                unsafe {
                    target_ptr = target_ptr.add(decoder_limit);
                }
            },
        }
    }

    if cfg!(debug_assertions) {
        assert_eq!(validity.set_bits(), 0);
    }

    let target_slice = unsafe { std::slice::from_raw_parts_mut(target_ptr, validity.len()) };
    target_slice.fill(B::zeroed());
    unsafe {
        target.set_len(end_length);
    }

    Ok(())
}

#[inline(never)]
pub fn decode_masked_optional_dict<B: AlignedBytes>(
    mut values: HybridRleDecoder<'_>,
    dict: &[B],
    filter: &Bitmap,
    validity: &Bitmap,
    target: &mut Vec<B>,
) -> ParquetResult<()> {
    let num_rows = filter.set_bits();
    let num_valid_values = validity.set_bits();

    // Dispatch to the non-filter kernel if all rows are needed anyway.
    if num_rows == filter.len() {
        return decode_optional_dict(values, dict, validity, target);
    }

    // Dispatch to the required kernel if all rows are valid anyway.
    if num_valid_values == validity.len() {
        return decode_masked_required_dict(values, dict, filter, target);
    }

    if dict.is_empty() && num_valid_values > 0 {
        return Err(oob_dict_idx());
    }

    debug_assert_eq!(filter.len(), validity.len());
    assert!(num_valid_values <= values.len());
    let start_length = target.len();

    target.reserve(num_rows);
    let mut target_ptr = unsafe { target.as_mut_ptr().add(start_length) };

    let mut filter = BitMask::from_bitmap(filter);
    let mut validity = BitMask::from_bitmap(validity);

    values.limit_to(num_valid_values);
    let mut values_buffer = [0u32; 128];
    let values_buffer = &mut values_buffer;

    let mut num_rows_left = num_rows;

    for chunk in values.into_chunk_iter() {
        // Early stop if we have no more rows to load.
        if num_rows_left == 0 {
            break;
        }

        match chunk? {
            HybridRleChunk::Rle(value, size) => {
                if size == 0 {
                    continue;
                }

                // If we know that we have `size` times `value` that we can append, but there might
                // be nulls in between those values.
                //
                // 1. See how many `num_rows = valid + invalid` values `size` would entail. This is
                //    done with `num_bits_before_nth_one` on the validity mask.
                // 2. Fill `num_rows` values into the target buffer.
                // 3. Advance the validity mask by `num_rows` values.

                let num_chunk_values = validity.nth_set_bit_idx(size, 0).unwrap_or(validity.len());

                let current_filter;
                (_, validity) = unsafe { validity.split_at_unchecked(num_chunk_values) };
                (current_filter, filter) = unsafe { filter.split_at_unchecked(num_chunk_values) };

                let num_chunk_rows = current_filter.set_bits();

                if num_chunk_rows > 0 {
                    let target_slice;
                    // SAFETY:
                    // Given `filter_iter` before the `advance_by_bits`.
                    //
                    // 1. `target_ptr..target_ptr + filter_iter.count_ones()` is allocated
                    // 2. `num_chunk_rows < filter_iter.count_ones()`
                    unsafe {
                        target_slice = std::slice::from_raw_parts_mut(target_ptr, num_chunk_rows);
                        target_ptr = target_ptr.add(num_chunk_rows);
                    }

                    let Some(value) = dict.get(value as usize) else {
                        return Err(oob_dict_idx());
                    };

                    target_slice.fill(*value);
                    num_rows_left -= num_chunk_rows;
                }
            },
            HybridRleChunk::Bitpacked(mut decoder) => {
                // For bitpacked we do the following:
                // 1. See how many rows are encoded by this `decoder`.
                // 2. Go through the filter and validity 56 bits at a time and:
                //    0. If filter bits are 0, skip the chunk entirely.
                //    1. Buffer enough values so that we can branchlessly decode with the filter
                //       and validity.
                //    2. Decode with filter and validity.
                // 3. Decode remainder.

                let size = decoder.len();
                let mut chunked = decoder.chunked();

                let num_chunk_values = validity.nth_set_bit_idx(size, 0).unwrap_or(validity.len());

                let mut buffer_part_idx = 0;
                let mut values_offset = 0;
                let mut num_buffered: usize = 0;
                let mut skip_values = 0;

                let current_filter;
                let current_validity;

                (current_filter, filter) = unsafe { filter.split_at_unchecked(num_chunk_values) };
                (current_validity, validity) =
                    unsafe { validity.split_at_unchecked(num_chunk_values) };

                let mut iter = |mut f: u64, mut v: u64| {
                    // Skip chunk if we don't any values from here.
                    if f == 0 {
                        skip_values += v.count_ones() as usize;
                        return ParquetResult::Ok(());
                    }

                    // Skip over already buffered items.
                    let num_buffered_skipped = skip_values.min(num_buffered);
                    values_offset += num_buffered_skipped;
                    num_buffered -= num_buffered_skipped;
                    skip_values -= num_buffered_skipped;

                    // If we skipped plenty already, just skip decoding those chunks instead of
                    // decoding them and throwing them away.
                    chunked.decoder.skip_chunks(skip_values / 32);
                    // The leftovers we have to decode but we can also just skip.
                    skip_values %= 32;

                    while num_buffered < v.count_ones() as usize {
                        let buffer_part = <&mut [u32; 32]>::try_from(
                            &mut values_buffer[buffer_part_idx * 32..][..32],
                        )
                        .unwrap();
                        let num_added = chunked.next_into(buffer_part).unwrap();

                        verify_dict_indices(buffer_part, dict.len())?;

                        let skip_chunk_values = skip_values.min(num_added);

                        values_offset += skip_chunk_values;
                        num_buffered += num_added - skip_chunk_values;
                        skip_values -= skip_chunk_values;

                        buffer_part_idx += 1;
                        buffer_part_idx %= 4;
                    }

                    let mut num_read = 0;
                    let mut num_written = 0;

                    while f != 0 {
                        let offset = f.trailing_zeros();

                        num_read += (v & (1u64 << offset).wrapping_sub(1)).count_ones() as usize;
                        v >>= offset;

                        let idx = values_buffer[(values_offset + num_read) % 128];
                        // SAFETY:
                        // 1. `values_buffer` starts out as only zeros, which we know is in the
                        //    dictionary following the original `dict.is_empty` check.
                        // 2. Each time we write to `values_buffer`, it is followed by a
                        //    `verify_dict_indices`.
                        let value = unsafe { dict.get_unchecked(idx as usize) };
                        let value = *value;
                        unsafe { target_ptr.add(num_written).write(value) };

                        num_written += 1;
                        num_read += (v & 1) as usize;

                        f >>= offset + 1; // Clear least significant bit.
                        v >>= 1;
                    }

                    num_read += v.count_ones() as usize;

                    values_offset += num_read;
                    values_offset %= 128;
                    num_buffered -= num_read;
                    unsafe {
                        target_ptr = target_ptr.add(num_written);
                    }
                    num_rows_left -= num_written;

                    ParquetResult::Ok(())
                };

                let mut f_iter = current_filter.fast_iter_u56();
                let mut v_iter = current_validity.fast_iter_u56();

                for (f, v) in f_iter.by_ref().zip(v_iter.by_ref()) {
                    iter(f, v)?;
                }

                let (f, fl) = f_iter.remainder();
                let (v, vl) = v_iter.remainder();

                assert_eq!(fl, vl);

                iter(f, v)?;
            },
        }
    }

    if cfg!(debug_assertions) {
        assert_eq!(validity.set_bits(), 0);
    }

    let target_slice = unsafe { std::slice::from_raw_parts_mut(target_ptr, num_rows_left) };
    target_slice.fill(B::zeroed());
    unsafe {
        target.set_len(start_length + num_rows);
    }

    Ok(())
}

#[inline(never)]
pub fn decode_masked_required_dict<B: AlignedBytes>(
    mut values: HybridRleDecoder<'_>,
    dict: &[B],
    filter: &Bitmap,
    target: &mut Vec<B>,
) -> ParquetResult<()> {
    let num_rows = filter.set_bits();

    // Dispatch to the non-filter kernel if all rows are needed anyway.
    if num_rows == filter.len() {
        values.limit_to(filter.len());
        return decode_required_dict(values, dict, target);
    }

    if dict.is_empty() && !filter.is_empty() {
        return Err(oob_dict_idx());
    }

    let start_length = target.len();

    target.reserve(num_rows);
    let mut target_ptr = unsafe { target.as_mut_ptr().add(start_length) };

    let mut filter = BitMask::from_bitmap(filter);

    values.limit_to(filter.len());
    let mut values_buffer = [0u32; 128];
    let values_buffer = &mut values_buffer;

    let mut num_rows_left = num_rows;

    for chunk in values.into_chunk_iter() {
        if num_rows_left == 0 {
            break;
        }

        match chunk? {
            HybridRleChunk::Rle(value, size) => {
                if size == 0 {
                    continue;
                }

                let size = size.min(filter.len());

                // If we know that we have `size` times `value` that we can append, but there might
                // be nulls in between those values.
                //
                // 1. See how many `num_rows = valid + invalid` values `size` would entail. This is
                //    done with `num_bits_before_nth_one` on the validity mask.
                // 2. Fill `num_rows` values into the target buffer.
                // 3. Advance the validity mask by `num_rows` values.

                let current_filter;

                (current_filter, filter) = unsafe { filter.split_at_unchecked(size) };
                let num_chunk_rows = current_filter.set_bits();

                if num_chunk_rows > 0 {
                    let target_slice;
                    // SAFETY:
                    // Given `filter_iter` before the `advance_by_bits`.
                    //
                    // 1. `target_ptr..target_ptr + filter_iter.count_ones()` is allocated
                    // 2. `num_chunk_rows < filter_iter.count_ones()`
                    unsafe {
                        target_slice = std::slice::from_raw_parts_mut(target_ptr, num_chunk_rows);
                        target_ptr = target_ptr.add(num_chunk_rows);
                    }

                    let Some(value) = dict.get(value as usize) else {
                        return Err(oob_dict_idx());
                    };

                    target_slice.fill(*value);
                    num_rows_left -= num_chunk_rows;
                }
            },
            HybridRleChunk::Bitpacked(mut decoder) => {
                let size = decoder.len().min(filter.len());
                let mut chunked = decoder.chunked();

                let mut buffer_part_idx = 0;
                let mut values_offset = 0;
                let mut num_buffered: usize = 0;
                let mut skip_values = 0;

                let current_filter;

                (current_filter, filter) = unsafe { filter.split_at_unchecked(size) };

                let mut iter = |mut f: u64, len: usize| {
                    debug_assert!(len <= 64);

                    // Skip chunk if we don't any values from here.
                    if f == 0 {
                        skip_values += len;
                        return ParquetResult::Ok(());
                    }

                    // Skip over already buffered items.
                    let num_buffered_skipped = skip_values.min(num_buffered);
                    values_offset += num_buffered_skipped;
                    num_buffered -= num_buffered_skipped;
                    skip_values -= num_buffered_skipped;

                    // If we skipped plenty already, just skip decoding those chunks instead of
                    // decoding them and throwing them away.
                    chunked.decoder.skip_chunks(skip_values / 32);
                    // The leftovers we have to decode but we can also just skip.
                    skip_values %= 32;

                    while num_buffered < len {
                        let buffer_part = <&mut [u32; 32]>::try_from(
                            &mut values_buffer[buffer_part_idx * 32..][..32],
                        )
                        .unwrap();
                        let num_added = chunked.next_into(buffer_part).unwrap();

                        verify_dict_indices(buffer_part, dict.len())?;

                        let skip_chunk_values = skip_values.min(num_added);

                        values_offset += skip_chunk_values;
                        num_buffered += num_added - skip_chunk_values;
                        skip_values -= skip_chunk_values;

                        buffer_part_idx += 1;
                        buffer_part_idx %= 4;
                    }

                    let mut num_read = 0;
                    let mut num_written = 0;

                    while f != 0 {
                        let offset = f.trailing_zeros() as usize;

                        num_read += offset;

                        let idx = values_buffer[(values_offset + num_read) % 128];
                        // SAFETY:
                        // 1. `values_buffer` starts out as only zeros, which we know is in the
                        //    dictionary following the original `dict.is_empty` check.
                        // 2. Each time we write to `values_buffer`, it is followed by a
                        //    `verify_dict_indices`.
                        let value = *unsafe { dict.get_unchecked(idx as usize) };
                        unsafe { target_ptr.add(num_written).write(value) };

                        num_written += 1;
                        num_read += 1;

                        f >>= offset + 1; // Clear least significant bit.
                    }

                    values_offset += len;
                    values_offset %= 128;
                    num_buffered -= len;
                    unsafe {
                        target_ptr = target_ptr.add(num_written);
                    }
                    num_rows_left -= num_written;

                    ParquetResult::Ok(())
                };

                let mut f_iter = current_filter.fast_iter_u56();

                for f in f_iter.by_ref() {
                    iter(f, 56)?;
                }

                let (f, fl) = f_iter.remainder();

                iter(f, fl)?;
            },
        }
    }

    unsafe {
        target.set_len(start_length + num_rows);
    }

    Ok(())
}
