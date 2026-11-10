use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use crate::unified_exec::format_output_omission_marker;
use std::collections::VecDeque;

/// A capped buffer that preserves a stable prefix ("head") and suffix ("tail"),
/// dropping the middle once it exceeds the configured maximum. The buffer is
/// symmetric meaning 50% of the capacity is allocated to the head and 50% is
/// allocated to the tail.
#[derive(Debug, Default)]
#[cfg_attr(test, derive(Eq, PartialEq))]
pub(crate) struct HeadTailBuffer<const MAX_BYTES: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES> {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted_bytes: usize,
    /// ByteAsk: a complete copy on disk, started the moment this buffer would begin to
    /// drop its middle. Absent when nothing was dropped,
    /// when there is no spill directory, or when the copy could not be made.
    tee: TeeSlot,
}

/// Holds the tee without taking part in equality: two buffers with the same bytes are
/// equal whatever happened on disk.
#[derive(Debug, Default)]
pub(crate) struct TeeSlot(Option<byteask_output_reducers::spill::Tee>);

impl PartialEq for TeeSlot {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}
impl Eq for TeeSlot {}

/// Buffers smaller than this are test fixtures; never tee them.
const BYTEASK_TEE_MIN_CAPACITY: usize = 64 * 1024;

impl<const MAX_BYTES: usize> HeadTailBuffer<MAX_BYTES> {
    /// Take the complete on-disk copy, if one was made.
    pub(crate) fn take_tee(&mut self) -> Option<byteask_output_reducers::spill::Tee> {
        self.tee.0.take()
    }

    /// Called BEFORE `incoming` bytes are added. Starts the tee at the last moment the
    /// buffer still holds everything, then mirrors every later byte.
    fn byteask_tee_before_push(&mut self, incoming: usize) {
        if MAX_BYTES < BYTEASK_TEE_MIN_CAPACITY || self.tee.0.is_some() {
            return;
        }
        if self.omitted_bytes == 0 && self.retained_bytes().saturating_add(incoming) > MAX_BYTES {
            self.tee.0 = byteask_output_reducers::spill::Tee::start(&self.to_bytes());
        }
    }

    const HEAD_BUDGET: usize = MAX_BYTES / 2;
    const TAIL_BUDGET: usize = MAX_BYTES.saturating_sub(Self::HEAD_BUDGET);

    // Used for tests.
    #[allow(dead_code)]
    /// Total bytes currently retained by the buffer (head + tail).
    pub(crate) fn retained_bytes(&self) -> usize {
        self.head.len().saturating_add(self.tail.len())
    }

    // Used for tests.
    #[allow(dead_code)]
    /// Total bytes that were dropped from the middle due to the size cap.
    pub(crate) fn omitted_bytes(&self) -> usize {
        self.omitted_bytes
    }

    /// Total bytes observed by the buffer, including bytes omitted by the cap.
    pub(crate) fn total_bytes(&self) -> usize {
        self.retained_bytes().saturating_add(self.omitted_bytes)
    }

    /// Append a chunk of bytes to the buffer.
    ///
    /// Bytes are first added to the head until the head budget is full; any
    /// remaining bytes are added to the tail, with older tail bytes being
    /// dropped to preserve the tail budget.
    pub(crate) fn push_chunk(&mut self, chunk: &[u8]) {
        self.byteask_tee_before_push(chunk.len());
        if let Some(tee) = self.tee.0.as_mut() {
            tee.append(chunk);
        }
        let chunk = self.fill_head(chunk);
        self.push_tail(chunk);
    }

    /// Fill the stable prefix and return the bytes that did not fit.
    fn fill_head<'a>(&mut self, chunk: &'a [u8]) -> &'a [u8] {
        let Self {
            head,
            tail: _,
            omitted_bytes: _,
            tee: _,
        } = self;

        let remaining_head = Self::HEAD_BUDGET.saturating_sub(head.len());
        // A shorter chunk fits entirely in the head.
        let (chunk_head, chunk_tail) = chunk
            .split_at_checked(remaining_head)
            .unwrap_or((chunk, &[]));
        head.extend_from_slice(chunk_head);
        chunk_tail
    }

    /// Append bytes known not to belong in the head, keeping the newest tail bytes.
    fn push_tail(&mut self, chunk: &[u8]) {
        let Self {
            head: _,
            tail,
            omitted_bytes,
            tee: _,
        } = self;

        let remaining_tail = Self::TAIL_BUDGET.saturating_sub(tail.len());
        let excess_tail = chunk.len().saturating_sub(remaining_tail);
        *omitted_bytes = omitted_bytes.saturating_add(excess_tail);

        // Discard old tail bytes first, then skip any excess incoming bytes.
        let chunk = match excess_tail.checked_sub(tail.len()) {
            None => {
                tail.drain(..excess_tail);
                chunk
            }
            Some(skip) => {
                tail.clear();
                &chunk[skip..]
            }
        };
        tail.extend(chunk);
    }

    /// Return the retained output as a single byte vector.
    ///
    /// The output is formed by concatenating head chunks, then tail chunks.
    /// Omitted bytes are not represented in the returned value.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.retained_bytes());
        out.extend_from_slice(&self.head);
        out.extend(self.tail.iter().copied());
        out
    }

    /// Return the retained output with an explicit marker between the head and
    /// tail when bytes were omitted.
    pub(crate) fn to_bytes_with_omission_marker(&self) -> Vec<u8> {
        if self.omitted_bytes == 0 {
            return self.to_bytes();
        }

        let marker = format_output_omission_marker(self.omitted_bytes);
        let marker_delimiter_bytes = 2;
        let mut out = Vec::with_capacity(
            self.retained_bytes()
                .saturating_add(marker.len())
                .saturating_add(marker_delimiter_bytes),
        );
        out.extend_from_slice(&self.head);
        out.push(b'\n');
        out.extend_from_slice(marker.as_bytes());
        out.push(b'\n');
        out.extend(self.tail.iter().copied());
        out
    }

    /// Append a later buffer with the same budget. This preserves the summary
    /// of the original concatenated output, including its omission count.
    pub(crate) fn push_buffer(&mut self, mut buffer: Self) {
        // ByteAsk tee: keep the on-disk copy complete across merges.
        let other_tee = buffer.tee.0.take();
        self.byteask_tee_before_push(buffer.total_bytes());
        if self.tee.0.is_none() && other_tee.is_some() && self.omitted_bytes == 0 {
            self.tee.0 = byteask_output_reducers::spill::Tee::start(&self.to_bytes());
        }
        if let Some(tee) = self.tee.0.as_mut() {
            match other_tee {
                Some(other) => tee.append_tee(other),
                None => {
                    tee.append(&buffer.to_bytes());
                    tee.note_gap(buffer.omitted_bytes as u64);
                }
            }
        }
        let Self {
            head,
            tail,
            omitted_bytes,
            tee: _,
        } = buffer;

        self.omitted_bytes = self.omitted_bytes.saturating_add(omitted_bytes);

        // Preserve an existing prefix; otherwise reuse the source head.
        let overflow = if self.head.is_empty() {
            self.head = head;
            &[]
        } else {
            self.fill_head(&head)
        };

        // A full source tail displaces both the old tail and the unused source head.
        if tail.len() == Self::TAIL_BUDGET {
            self.omitted_bytes = self
                .omitted_bytes
                .saturating_add(self.tail.len())
                .saturating_add(overflow.len());
            self.tail = tail;
        } else {
            self.push_tail(overflow);
            // An empty destination can take a partial source tail without copying it.
            if self.tail.is_empty() {
                self.tail = tail;
            } else {
                // A nonempty source tail means its head, and now ours, is full.
                let (first, second) = tail.as_slices();
                self.push_tail(first);
                self.push_tail(second);
            }
        }
    }
}

#[cfg(test)]
#[path = "head_tail_buffer_tests.rs"]
mod tests;
