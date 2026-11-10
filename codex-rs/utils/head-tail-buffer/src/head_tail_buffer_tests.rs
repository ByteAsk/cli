use super::HeadTailBuffer;

use pretty_assertions::assert_eq;

#[test]
fn keeps_prefix_and_suffix_when_over_budget() {
    let mut buf = HeadTailBuffer::<10>::default();

    buf.push_chunk(b"0123456789");
    assert_eq!(buf.omitted_bytes(), 0);

    // Exceeds max by 2; we should keep head+tail and omit the middle.
    buf.push_chunk(b"ab");
    assert!(buf.omitted_bytes() > 0);

    let rendered = String::from_utf8_lossy(&buf.to_bytes()).to_string();
    assert!(rendered.starts_with("01234"));
    assert!(rendered.ends_with("89ab"));
    assert_eq!(
        String::from_utf8_lossy(&buf.to_bytes_with_omission_marker()),
        "01234\n... 2 bytes omitted ...\n789ab"
    );
}

#[test]
fn max_bytes_zero_drops_everything() {
    let mut buf = HeadTailBuffer::<0>::default();
    buf.push_chunk(b"abc");

    assert_eq!(buf.retained_bytes(), 0);
    assert_eq!(buf.omitted_bytes(), 3);
    assert_eq!(buf.to_bytes(), b"");
}

#[test]
fn head_budget_zero_keeps_only_last_byte_in_tail() {
    let mut buf = HeadTailBuffer::<1>::default();
    buf.push_chunk(b"abc");

    assert_eq!(buf.retained_bytes(), 1);
    assert_eq!(buf.omitted_bytes(), 2);
    assert_eq!(buf.to_bytes(), b"c");
}

#[test]
fn chunk_larger_than_tail_budget_keeps_only_tail_end() {
    let mut buf = HeadTailBuffer::<10>::default();
    buf.push_chunk(b"0123456789");

    // Tail budget is 5 bytes. This chunk should replace the tail and keep only its last 5 bytes.
    buf.push_chunk(b"ABCDEFGHIJK");

    let out = String::from_utf8_lossy(&buf.to_bytes()).to_string();
    assert!(out.starts_with("01234"));
    assert!(out.ends_with("GHIJK"));
    assert!(buf.omitted_bytes() > 0);
}

#[test]
fn fills_head_then_tail_across_multiple_chunks() {
    let mut buf = HeadTailBuffer::<10>::default();

    // Fill the 5-byte head budget across multiple chunks.
    buf.push_chunk(b"01");
    buf.push_chunk(b"234");
    assert_eq!(buf.to_bytes(), b"01234");

    // Then fill the 5-byte tail budget.
    buf.push_chunk(b"567");
    buf.push_chunk(b"89");
    assert_eq!(buf.to_bytes(), b"0123456789");
    assert_eq!(buf.omitted_bytes(), 0);

    // One more byte causes the tail to drop its oldest byte.
    buf.push_chunk(b"a");
    assert_eq!(buf.to_bytes(), b"012346789a");
    assert_eq!(buf.omitted_bytes(), 1);
}

#[test]
fn empty_and_tiny_chunks_have_bounded_metadata() {
    let mut buf = HeadTailBuffer::<10>::default();

    for byte in b"0123456789ab" {
        buf.push_chunk(&[]);
        buf.push_chunk(&[*byte]);
    }

    assert_eq!(buf.retained_bytes(), 10);
    assert_eq!(buf.omitted_bytes(), 2);
}

// ---- ByteAsk spill tee --------------------------------

#[test]
fn byteask_the_tee_keeps_the_complete_output_once_the_middle_is_dropped() {
    const CAP: usize = 128 * 1024;
    // The spill directory is process-wide: use whichever was set first, and make sure it
    // still exists (a test that owned a temp dir may have removed it).
    // Kept on purpose: the directory is process-wide, so it must outlive this test.
    let tmp = tempfile::tempdir().expect("tempdir").keep();
    byteask_output_reducers::spill::init(&tmp);
    let dir = byteask_output_reducers::spill::dir()
        .expect("spill dir")
        .to_path_buf();
    std::fs::create_dir_all(&dir).expect("spill dir exists");

    let mut full: Vec<u8> = Vec::new();
    let mut buf = HeadTailBuffer::<CAP>::default();
    for i in 0..400u32 {
        let chunk = format!("line {i:06} {}\n", "x".repeat(1000)).into_bytes();
        full.extend_from_slice(&chunk);
        buf.push_chunk(&chunk);
    }
    assert!(
        buf.omitted_bytes() > 0,
        "the in-memory buffer did drop its middle"
    );
    assert!(buf.to_bytes().len() < full.len());

    // A second buffer merged in, as the collector does between polls.
    let mut later = HeadTailBuffer::<CAP>::default();
    later.push_chunk(b"the very last line\n");
    full.extend_from_slice(b"the very last line\n");
    buf.push_buffer(later);

    let tee = buf.take_tee().expect("a tee was started");
    assert_eq!(tee.lost(), 0);
    let path = tee.adopt("call_tee_core_c1").expect("adopt");
    assert_eq!(std::fs::read(&path).expect("read"), full);
    // The serializer's later write() of the TRUNCATED bytes must not replace it.
    let again = byteask_output_reducers::spill::write("call_tee_core_c1", &buf.to_bytes());
    assert_eq!(again, Some(path.clone()));
    assert_eq!(std::fs::read(&path).expect("read").len(), full.len());
}

#[test]
fn byteask_small_outputs_never_start_a_tee() {
    let mut buf = HeadTailBuffer::<{ 128 * 1024 }>::default();
    buf.push_chunk(b"short\n");
    assert!(buf.take_tee().is_none());
    // Tiny fixture buffers are never teed, whatever they drop.
    let mut tiny = HeadTailBuffer::<10>::default();
    tiny.push_chunk(b"0123456789abcdefghij");
    assert!(tiny.take_tee().is_none());
}
