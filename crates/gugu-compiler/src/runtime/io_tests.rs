use super::coroutine::CoroutineState;
use super::foreign::{AdmitOutcome, ForeignWorld, IoClass, PollerFamily};
use super::io::{
    Cursor, INVALID_DATA, INVALID_INPUT, IoError, IoErrors, IoWait, IoWaitResult, ReadBytes,
    Scripted, SeekFrom, UNEXPECTED_EOF, WRITE_ZERO, admit_blocking, copy, fill_buffer, read_buffer,
    read_exact, read_to_string, write_all, write_buffer,
};
use super::text::{CowBytes, TextFault};

#[test]
fn cursor_reads_short_then_finishes_with_zero() {
    let mut cursor = Cursor::new(b"abcdef".to_vec());
    let mut first = [0; 4];
    assert_eq!(cursor.read(&mut first).expect("短读"), 4);
    assert_eq!(&first, b"abcd");
    let mut second = [0; 4];
    assert_eq!(cursor.read(&mut second).expect("读完"), 2);
    assert_eq!(&second[..2], b"ef");
    assert_eq!(cursor.read(&mut second).expect("EOF"), 0);
}

#[test]
fn read_exact_keeps_prefix_when_eof_arrives_early() {
    let mut reader = Scripted::new(vec![Ok(b"ab".to_vec()), Ok(Vec::new())], Vec::new());
    let mut destination = [0; 4];
    let error = read_exact(&mut reader, &mut destination).expect_err("中途 EOF");
    assert_eq!(error.kind_code(), UNEXPECTED_EOF);
    assert_eq!(&destination[..2], b"ab");
}

#[test]
fn write_all_turns_zero_into_write_zero_and_skips_empty_input() {
    let mut writer = Scripted::new(Vec::new(), vec![Ok(0)]);
    let error = write_all(&mut writer, b"ab").expect_err("零写");
    assert_eq!(error.kind_code(), WRITE_ZERO);
    assert_eq!(writer.writes_done(), 1);
    let mut idle = Scripted::new(Vec::new(), Vec::new());
    write_all(&mut idle, b"").expect("空输入");
    assert_eq!(idle.writes_done(), 0);
}

#[test]
fn copy_accumulates_short_writes() {
    let mut reader = Cursor::new(b"abcd".to_vec());
    let mut writer = Scripted::new(Vec::new(), vec![Ok(1), Ok(1), Ok(2)]);
    assert_eq!(copy(&mut reader, &mut writer).expect("复制"), 4);
    assert_eq!(writer.writes_done(), 3);
}

#[test]
fn read_to_string_rejects_invalid_utf8() {
    let mut reader = Cursor::new(vec![0xFF]);
    let error = read_to_string(&mut reader).expect_err("非法 UTF-8");
    assert_eq!(error.kind_code(), INVALID_DATA);
}

#[test]
fn with_source_exposes_two_layers_and_rejects_a_cycle() {
    let mut errors = IoErrors::new();
    let root = IoError::bare(WRITE_ZERO);
    let first = errors.with_source(root, IoError::bare(UNEXPECTED_EOF));
    let second = errors.with_source(first, root);
    let outer = errors.source(second).expect("第一层");
    assert_eq!(outer.kind_code(), WRITE_ZERO);
    assert_eq!(errors.source(outer), None);
    assert_eq!(errors.kind(second).kind_code(), WRITE_ZERO);
    let cycled = errors.with_source(second, IoError::bare(WRITE_ZERO));
    assert_eq!(cycled.kind_code(), INVALID_INPUT);
    assert!(cycled.source_index().is_none());
}

#[test]
fn negative_read_buffer_is_a_text_fault() {
    let mut reader = Cursor::new(b"ab".to_vec());
    let mut buffer = CowBytes::new();
    assert_eq!(
        read_buffer(&mut buffer, -1, &mut reader),
        Err(TextFault::Negative)
    );
}

#[test]
fn cursor_seek_rejects_negative_underflow_and_reads_zero_past_end() {
    let mut cursor = Cursor::new(b"ab".to_vec());
    assert!(cursor.seek(SeekFrom::End(-3)).is_err());
    assert_eq!(cursor.seek(SeekFrom::Start(4)).expect("越过尾部"), 4);
    let mut destination = [0; 1];
    assert_eq!(cursor.read(&mut destination).expect("空洞"), 0);
}

#[test]
fn wait_submits_close_cancel_and_timeout_once() {
    let mut closed = IoWait::new(10, Some(10), true, true);
    closed.grant_credit();
    assert_eq!(closed.poll(4, false), IoWaitResult::Closed);
    assert_eq!(closed.poll(4, false), IoWaitResult::Closed);
    let mut cancelled = IoWait::new(10, Some(10), true, false);
    cancelled.grant_credit();
    assert_eq!(cancelled.poll(4, false), IoWaitResult::Cancelled);
    let mut timed = IoWait::new(10, Some(10), false, false);
    timed.grant_credit();
    assert_eq!(timed.poll(4, false), IoWaitResult::TimedOut);
}

#[test]
fn fill_and_write_buffer_cover_zero_and_prefix() {
    let mut reader = Cursor::new(b"abcdef".to_vec());
    assert!(fill_buffer(&mut reader, 0).expect("空").is_empty());
    assert_eq!(reader.position(), 0);
    let filled = fill_buffer(&mut reader, 3).expect("前缀");
    assert_eq!(filled, b"abc");
    let mut source = CowBytes::new();
    source.extend_from_slice(b"xyz");
    let mut writer = Scripted::new(Vec::new(), vec![Ok(1)]);
    assert_eq!(write_buffer(&mut writer, &mut source).expect("一次写"), 1);
}

#[test]
fn blocking_admission_waits_without_credit_and_regular_files_stay_off_poller() {
    let mut world = ForeignWorld::new(2).expect("世界");
    let waiting = world.spawn_running().expect("等待者");
    assert_eq!(
        admit_blocking(&mut world, waiting, false).expect("无额度"),
        AdmitOutcome::Waiting
    );
    assert_eq!(
        world.lifecycle(waiting).expect("等待"),
        CoroutineState::Waiting
    );
    let ready = world.spawn_running().expect("就绪者");
    assert!(matches!(
        admit_blocking(&mut world, ready, true).expect("有额度"),
        AdmitOutcome::Attached { .. }
    ));
    let error = world
        .submit(PollerFamily::Linux, IoClass::RegularFile, 1)
        .expect_err("文件不进 poller");
    assert!(error.to_string().contains("BlockingBridge"));
}
