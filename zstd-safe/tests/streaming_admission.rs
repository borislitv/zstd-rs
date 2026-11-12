#![cfg(feature = "experimental")]
#![forbid(unsafe_code)]

use std::num::NonZeroU32;

use zstd_safe::{
    estimate_cstream_size, estimate_dstream_size, get_frame_header,
    is_error_kind, CCtx, CParameter, DCtx, DParameter, ErrorCode, FrameHeader,
    FrameHeaderStatus, FrameType, InBuffer, OutBuffer,
};

const WINDOW_LOG: u32 = 20;
const WINDOW_SIZE: usize = 1 << WINDOW_LOG;
const MAX_CONTENT: usize = 8 << 20;

// Fixed header-only format examples, not a parser or encoder implementation.
// 4 KiB content, 1 MiB window, dictionary 0x12345678, checksum present.
const METADATA_HEADER: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x87, 0x50, 0x78, 0x56, 0x34, 0x12, 0x00, 0x10,
    0x00, 0x00,
];
const TWO_MIB_WINDOW: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x58];
const HUGE_WINDOW: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0xa0];

fn header(frame: &[u8]) -> FrameHeader {
    match get_frame_header(frame).unwrap() {
        FrameHeaderStatus::Complete(header) => header,
        status => panic!("expected a complete header, got {:?}", status),
    }
}

#[test]
fn frame_header_reports_independent_metadata() {
    assert_eq!(
        header(METADATA_HEADER),
        FrameHeader {
            frame_type: FrameType::Zstandard,
            content_size: Some(4096),
            window_size: 1 << 20,
            dict_id: NonZeroU32::new(0x12345678),
            checksum: true,
            header_size: 14,
        }
    );
    let unknown = header(TWO_MIB_WINDOW);
    assert_eq!(unknown.content_size, None);
    assert_eq!(unknown.window_size, 2 << 20);
    assert_eq!(unknown.dict_id, None);
    assert!(!unknown.checksum);
    assert_eq!(header(HUGE_WINDOW).window_size, 1 << 30);
}

#[test]
fn frame_header_distinguishes_incomplete_from_corrupt() {
    for prefix in [&METADATA_HEADER[..0], &METADATA_HEADER[..4]] {
        assert_eq!(
            get_frame_header(prefix),
            Ok(FrameHeaderStatus::Incomplete { required_size: 5 })
        );
    }
    for prefix_len in 5..METADATA_HEADER.len() {
        assert_eq!(
            get_frame_header(&METADATA_HEADER[..prefix_len]),
            Ok(FrameHeaderStatus::Incomplete { required_size: 14 }),
            "prefix length {prefix_len}"
        );
    }
    let bad_magic = get_frame_header(b"not zstd").unwrap_err();
    assert!(is_error_kind(
        bad_magic,
        zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_prefix_unknown
    ));
    let mut reserved_bit = METADATA_HEADER.to_vec();
    reserved_bit[4] |= 0x08;
    let error = get_frame_header(&reserved_bit).unwrap_err();
    assert!(is_error_kind(
        error,
        zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_frameParameter_unsupported
    ));
}

#[test]
fn skippable_variant_is_not_a_dictionary_identifier() {
    let frame = [0x53, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3];
    assert_eq!(
        get_frame_header(&frame[..5]),
        Ok(FrameHeaderStatus::Incomplete { required_size: 8 })
    );
    assert_eq!(
        header(&frame),
        FrameHeader {
            frame_type: FrameType::Skippable { id: 3 },
            content_size: Some(3),
            window_size: 0,
            dict_id: None,
            checksum: false,
            header_size: 8,
        }
    );
}

#[test]
fn estimates_reject_unsupported_ranges_before_native_arithmetic() {
    for level in [i32::MIN, -1, 0, zstd_safe::max_c_level() + 1, i32::MAX] {
        let error = estimate_cstream_size(level).unwrap_err();
        assert!(is_error_kind(
            error,
            zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        ));
    }
    for window in [0, 1, (1 << zstd_safe::WINDOWLOG_MIN) - 1, usize::MAX] {
        let error = estimate_dstream_size(window).unwrap_err();
        assert!(is_error_kind(
            error,
            zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_parameter_outOfBound
        ));
    }
    assert!(estimate_cstream_size(1).unwrap() > 0);
    assert!(estimate_cstream_size(zstd_safe::max_c_level()).unwrap() > 0);
    let max_log = if usize::BITS == 32 {
        zstd_safe::WINDOWLOG_MAX_32
    } else {
        zstd_safe::WINDOWLOG_MAX_64
    };
    assert!(estimate_dstream_size(1 << zstd_safe::WINDOWLOG_MIN).is_ok());
    assert!(estimate_dstream_size(1 << max_log).is_ok());
    assert!(estimate_dstream_size((1 << max_log) + 1).is_err());
    // The native API takes bytes, including non-power-of-two windows.
    assert!(estimate_dstream_size((1 << zstd_safe::WINDOWLOG_MIN) + 1).is_ok());
}

fn assert_cctx_budget(context: &CCtx<'_>, budget: usize) {
    assert!(
        context.sizeof() <= budget,
        "compression context uses {} bytes, estimate {budget}",
        context.sizeof()
    );
}

fn encode(source: &[u8], content_size: bool, checksum: bool) -> Vec<u8> {
    assert!(source.len() <= MAX_CONTENT);
    // Compute the admission charge before context creation. A caller must also
    // reserve its own buffers, allocator overhead and concurrency allowance.
    let budget = estimate_cstream_size(3).unwrap();
    let mut context =
        CCtx::try_create().expect("compression context allocation");
    assert_cctx_budget(&context, budget);
    let mut peak = context.sizeof();
    for parameter in [
        CParameter::CompressionLevel(3),
        CParameter::WindowLog(WINDOW_LOG),
        CParameter::NbWorkers(0),
        CParameter::ContentSizeFlag(content_size),
        CParameter::ChecksumFlag(checksum),
    ] {
        context.set_parameter(parameter).unwrap();
    }
    context
        .set_pledged_src_size(Some(source.len() as u64))
        .unwrap();
    let mut frame = Vec::new();
    let mut scratch = [0u8; 4096];
    for chunk in source.chunks(4096) {
        let mut input = InBuffer::around(chunk);
        while input.pos() < chunk.len() {
            let before = input.pos();
            let mut output = OutBuffer::around(&mut scratch[..]);
            context.compress_stream(&mut output, &mut input).unwrap();
            assert!(input.pos() > before || output.pos() > 0);
            frame.extend_from_slice(output.as_slice());
            assert_cctx_budget(&context, budget);
            peak = peak.max(context.sizeof());
        }
    }
    loop {
        let mut output = OutBuffer::around(&mut scratch[..]);
        let remaining = context.end_stream(&mut output).unwrap();
        frame.extend_from_slice(output.as_slice());
        assert_cctx_budget(&context, budget);
        peak = peak.max(context.sizeof());
        if remaining == 0 {
            break;
        }
        assert!(output.pos() > 0, "encoder finish stalled");
    }
    println!(
        "encode source={} native_estimate={budget} observed_peak={peak}",
        source.len()
    );
    frame
}

#[derive(Debug, PartialEq, Eq)]
enum Rejected {
    Native(ErrorCode),
    Truncated,
    Window,
    Metadata,
    Trailing,
    OutputLimit,
}

// A small caller-side policy example exercising only public safe APIs. Header
// policy is deliberately not baked into the generic dependency's parser.
fn decode(
    frame: &[u8],
    expected: &[u8],
    input_chunk: usize,
    output_chunk: usize,
) -> Result<(), Rejected> {
    let budget = estimate_dstream_size(WINDOW_SIZE).unwrap();
    let metadata = match get_frame_header(frame).map_err(Rejected::Native)? {
        FrameHeaderStatus::Complete(header) => header,
        FrameHeaderStatus::Incomplete { .. } => {
            return Err(Rejected::Truncated)
        }
    };
    // Validate before allocating a DCtx, including the complete-frame shortcut.
    if metadata.window_size > WINDOW_SIZE as u64 {
        return Err(Rejected::Window);
    }
    if metadata.frame_type != FrameType::Zstandard
        || metadata.content_size != Some(expected.len() as u64)
        || expected.len() > MAX_CONTENT
        || metadata.dict_id.is_some()
        || !metadata.checksum
    {
        return Err(Rejected::Metadata);
    }
    let mut context =
        DCtx::try_create().expect("decompression context allocation");
    context
        .set_parameter(DParameter::WindowLogMax(WINDOW_LOG))
        .unwrap();
    assert!(context.sizeof() <= budget);
    let mut peak = context.sizeof();
    let mut consumed = 0;
    let mut decoded = 0;
    let mut scratch = [0u8; 4096];
    loop {
        let end = frame.len().min(consumed + input_chunk);
        let mut input = InBuffer::around(&frame[consumed..end]);
        // Leave room to detect one excess byte even at the output limit.
        let capacity = output_chunk.min(expected.len() - decoded + 1);
        let mut output = OutBuffer::around(&mut scratch[..capacity]);
        let remaining = context
            .decompress_stream(&mut output, &mut input)
            .map_err(Rejected::Native)?;
        assert!(
            context.sizeof() <= budget,
            "decompression context uses {} bytes, estimate {budget}",
            context.sizeof()
        );
        peak = peak.max(context.sizeof());
        consumed += input.pos();
        let next = decoded + output.pos();
        if next > expected.len() {
            return Err(Rejected::OutputLimit);
        }
        assert_eq!(output.as_slice(), &expected[decoded..next]);
        decoded = next;
        if remaining == 0 {
            println!(
                "decode source={} input_chunk={input_chunk} output_chunk={output_chunk} native_estimate={budget} observed_peak={peak}",
                expected.len()
            );
            if consumed != frame.len() {
                return Err(Rejected::Trailing);
            }
            return if decoded == expected.len() {
                Ok(())
            } else {
                Err(Rejected::Metadata)
            };
        }
        if input.pos() == 0 && output.pos() == 0 {
            return Err(Rejected::Truncated);
        }
    }
}

#[test]
fn stream_context_sizes_stay_within_estimates_at_selected_boundaries() {
    assert_eq!(zstd_safe::version_number(), 10507);
    println!("native_version={}", zstd_safe::version_string());
    println!(
        "rust_cctx_bytes={} rust_dctx_bytes={}",
        std::mem::size_of::<CCtx<'static>>(),
        std::mem::size_of::<DCtx<'static>>()
    );
    for size in [0, 4096, WINDOW_SIZE, 4 << 20, MAX_CONTENT] {
        let source: Vec<u8> = (0..size)
            .map(|i| (i.wrapping_mul(31) ^ (i >> 8)) as u8)
            .collect();
        let frame = encode(&source, true, true);
        let metadata = header(&frame);
        assert_eq!(metadata.content_size, Some(size as u64));
        assert!(metadata.window_size <= WINDOW_SIZE as u64);
        if size >= WINDOW_SIZE {
            assert_eq!(metadata.window_size, WINDOW_SIZE as u64);
        }
        assert!(metadata.checksum);
        assert_eq!(metadata.dict_id, None);
        // Different chunk boundaries exercise incremental parsing and draining.
        assert_eq!(decode(&frame, &source, 113, 4096), Ok(()), "size {size}");
        assert_eq!(decode(&frame, &source, 4096, 257), Ok(()), "size {size}");
    }
}

#[test]
fn encoded_checksum_and_content_size_flags_are_observable() {
    for content_size in [false, true] {
        for checksum in [false, true] {
            let frame = encode(b"metadata", content_size, checksum);
            let metadata = header(&frame);
            assert_eq!(metadata.checksum, checksum);
            assert_eq!(
                metadata.content_size,
                if content_size { Some(8) } else { None }
            );
            if !content_size || !checksum {
                assert_eq!(
                    decode(&frame, b"metadata", 7, 11),
                    Err(Rejected::Metadata)
                );
            }
        }
    }
}

#[test]
fn streaming_finish_rejects_an_incorrect_eight_mib_pledge() {
    let budget = estimate_cstream_size(3).unwrap();
    let mut context = CCtx::try_create().unwrap();
    context
        .set_parameter(CParameter::CompressionLevel(3))
        .unwrap();
    context
        .set_parameter(CParameter::WindowLog(WINDOW_LOG))
        .unwrap();
    context
        .set_pledged_src_size(Some(MAX_CONTENT as u64))
        .unwrap();
    let source = [42u8; 4096];
    let mut input = InBuffer::around(&source);
    let mut scratch = [0u8; 4096];
    while input.pos() < source.len() {
        let before = input.pos();
        let mut output = OutBuffer::around(&mut scratch[..]);
        context.compress_stream(&mut output, &mut input).unwrap();
        assert!(input.pos() > before || output.pos() > 0);
        assert_cctx_budget(&context, budget);
    }
    let error = context
        .end_stream(&mut OutBuffer::around(&mut scratch[..]))
        .unwrap_err();
    assert!(is_error_kind(
        error,
        zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_srcSize_wrong
    ));
    assert_cctx_budget(&context, budget);
}

#[test]
fn corrupt_truncated_and_trailing_frames_do_not_succeed() {
    let source = b"checksum-protected content";
    let frame = encode(source, true, true);
    let mut corrupt = frame.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    let error = decode(&corrupt, source, 7, 11).unwrap_err();
    match error {
        Rejected::Native(code) => assert!(is_error_kind(
            code,
            zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_checksum_wrong
        )),
        error => panic!("expected checksum error, got {:?}", error),
    }
    assert_eq!(
        decode(&frame[..frame.len() - 1], source, 7, 11),
        Err(Rejected::Truncated)
    );
    for suffix in [&b"garbage"[..], &frame[..]] {
        let mut trailing = frame.clone();
        trailing.extend_from_slice(suffix);
        assert_eq!(
            decode(&trailing, source, 4096, 4096),
            Err(Rejected::Trailing)
        );
    }
}

#[test]
fn header_policy_and_native_streaming_guard_reject_oversized_windows() {
    for frame in [TWO_MIB_WINDOW, HUGE_WINDOW] {
        assert_eq!(decode(frame, b"", 7, 11), Err(Rejected::Window));
    }
    // 2 MiB is below the native default, but above the selected 1 MiB bound.
    // Only feed the header, avoiding the native complete-frame shortcut.
    let mut context = DCtx::try_create().unwrap();
    context
        .set_parameter(DParameter::WindowLogMax(WINDOW_LOG))
        .unwrap();
    let mut input = InBuffer::around(TWO_MIB_WINDOW);
    let mut scratch = [0u8; 64];
    let error = context
        .decompress_stream(
            &mut OutBuffer::around(&mut scratch[..]),
            &mut input,
        )
        .unwrap_err();
    assert!(is_error_kind(
        error,
        zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_frameParameter_windowTooLarge
    ));
}
