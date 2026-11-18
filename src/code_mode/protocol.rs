//! What Arko and the code runner (its child process) say to each other,
//! over the child's standard input and output. The child first writes
//! [`MAGIC`], and from then each side writes frames, each a 4-byte
//! little-endian length and then that many bytes of UTF-8 JSON. Arko sends
//! one [`Start`], then an [`Answer`] for each [`Call`], and the child ends
//! with one [`Done`].

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the child writes before its first frame, so Arko can tell its
/// frames from anything printed before them.
pub const MAGIC: &[u8] = b"\0arko-code-runner 1\n";

/// The largest frame the child reads. An answer holds at most
/// [`super::MAX_ANSWER_BYTES`] of text, which JSON may escape to a few
/// times its size.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The largest frame Arko reads from the child, a call's arguments
/// ([`super::MAX_CALL_BYTES`]) with room for the rest. A [`Done`] is
/// smaller, since the child cuts what it holds.
pub const MAX_FRAME_FROM_CHILD_BYTES: usize = super::MAX_CALL_BYTES + 64 * 1024;

/// What a run is limited to, all set before any code runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Computing time, waits for tools not counted.
    pub compute_ms: u64,
    pub heap_bytes: u64,
    pub stack_bytes: u64,
    /// Tool calls the code may make.
    pub calls: u32,
    /// The memory the operating system lets the child have, its backstop
    /// for the heap's cap.
    pub os_memory_bytes: u64,
    /// The address space the operating system lets the child add to what
    /// it has as it starts.
    pub os_address_space_bytes: u64,
    /// The processor time the operating system lets the child have, its
    /// backstop for the computing time.
    pub cpu_seconds: u64,
}

/// Arko's frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToChild {
    Start(Start),
    Answer(Answer),
}

/// The run, Arko's first frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Start {
    /// The body of an async function.
    pub code: String,
    /// The tools `tools` has, by name.
    pub tools: Vec<String>,
    pub limits: Limits,
}

/// The answer to call `id`. `text` is the result, or with `ok` false why
/// it failed, and `refused` says a check refused it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub id: u64,
    pub ok: bool,
    pub text: String,
    #[serde(default)]
    pub refused: bool,
}

/// The child's frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromChild {
    Call(Call),
    Done(Done),
}

/// A call of tool `tool` with `args` (a JSON object), which code made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub id: u64,
    pub tool: String,
    pub args: Value,
}

/// How a run ended, with what it logged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub ended: Ended,
    /// The code's `console` lines, up to [`super::MAX_LOG_BYTES`].
    pub log: Vec<String>,
    /// The lines past that, left out.
    pub log_left_out: u64,
    /// The characters of the returned value the child left out, past
    /// [`super::MAX_RESULT_CHARS`].
    #[serde(default)]
    pub value_left_out: u64,
    /// The calls the code made.
    pub calls: u32,
    pub heap_bytes: u64,
    pub compute_ms: u64,
}

/// Why a run ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ended {
    /// The code returned `value` (`None` for nothing), and its calls ended.
    /// `unread` are the calls that failed whose results the code never
    /// read, each as `tool: message`.
    Returned {
        value: Option<String>,
        unread: Vec<String>,
    },
    /// The code threw, or with `compiling` didn't compile. `line` is the
    /// line in the code, `refused` says it threw a refused call's error, and
    /// `tool` names that call's tool.
    Threw {
        message: String,
        line: Option<u32>,
        compiling: bool,
        refused: bool,
        tool: Option<String>,
    },
    /// The heap reached its cap.
    OutOfMemory { line: Option<u32> },
    /// The code computed for as long as a run may.
    ComputeLimit,
    /// The code made more calls than a run may.
    CallLimit,
    /// The code waits on a promise that nothing settles.
    Unsettled,
    /// The runner couldn't run the code.
    Failed { message: String },
}

/// Writes `message` as a frame.
pub fn write_frame(out: &mut impl Write, message: &impl Serialize) -> std::io::Result<()> {
    let body = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    let length = u32::try_from(body.len()).map_err(std::io::Error::other)?;
    out.write_all(&length.to_le_bytes())?;
    out.write_all(&body)?;
    out.flush()
}

/// Reads a frame's message, `None` at the end of the input before a frame
/// begins.
pub fn read_frame<T: for<'de> Deserialize<'de>>(
    input: &mut impl Read,
) -> std::io::Result<Option<T>> {
    let mut length = [0; 4];
    match input.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(std::io::Error::other(format!(
            "a frame of {length} bytes, over the {MAX_FRAME_BYTES} a frame may have"
        )));
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_messages_both_ways() {
        let start = ToChild::Start(Start {
            code: "return 1;".into(),
            tools: vec!["read_file".into()],
            limits: Limits {
                compute_ms: 1,
                heap_bytes: 2,
                stack_bytes: 3,
                calls: 4,
                os_memory_bytes: 5,
                os_address_space_bytes: 7,
                cpu_seconds: 6,
            },
        });
        let done = FromChild::Done(Done {
            ended: Ended::Returned {
                value: Some("1".into()),
                unread: Vec::new(),
            },
            log: vec!["a line".into()],
            log_left_out: 0,
            value_left_out: 0,
            calls: 0,
            heap_bytes: 10,
            compute_ms: 0,
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &start).unwrap();
        write_frame(&mut bytes, &done).unwrap();
        let mut input = bytes.as_slice();
        assert_eq!(read_frame::<ToChild>(&mut input).unwrap(), Some(start));
        assert_eq!(read_frame::<FromChild>(&mut input).unwrap(), Some(done));
        assert_eq!(read_frame::<FromChild>(&mut input).unwrap(), None);
        // A length past the limit is refused before anything is read.
        let mut huge = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes().to_vec();
        huge.extend_from_slice(b"{}");
        assert!(read_frame::<FromChild>(&mut huge.as_slice()).is_err());
    }
}
