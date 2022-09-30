use anyhow::{anyhow, bail, Result};
use shared_types::DataRoot;
use std::collections::HashMap;

/// The segment status in sliding window
#[derive(PartialEq, Eq, Debug)]
enum SlotStatus {
    Writing,
    Finished,
}

/// Sliding window is used to control the concurrent uploading process of a file.
/// Bounded window allows segments to be uploaded concurrenly, while having a capacity limit on writing threads per file
/// Meanwhile, the left_boundary field records how many segments have been uploaded
struct CtrlWindow {
    size: usize,
    left_boundary: usize,
    slots: HashMap<usize, SlotStatus>,
}

impl CtrlWindow {
    fn new(size: usize) -> Self {
        CtrlWindow {
            size,
            left_boundary: 0,
            slots: HashMap::default(),
        }
    }

    fn check_duplicate(&self, index: usize) -> bool {
        if index < self.left_boundary {
            return true;
        }

        if self.slots.contains_key(&index) {
            return true;
        }

        false
    }

    //Should call check_duplicate and handle the duplicated case before calling this function
    //This function assumes that there are no duplicate slots in the window
    fn start_writing(&mut self, index: usize) -> Result<()> {
        assert!(index >= self.left_boundary);

        if index >= self.left_boundary + self.size {
            bail!(
                "index exceeds window limit, index = {}, left_boundary = {}, window_size = {}",
                index,
                self.left_boundary,
                self.size
            );
        }

        assert!(!self.slots.contains_key(&index));
        self.slots.insert(index, SlotStatus::Writing);

        Ok(())
    }

    fn rollback_writing(&mut self, index: usize) {
        let slot_status = self.slots.remove(&index).unwrap();
        assert_eq!(slot_status, SlotStatus::Writing);
    }

    fn finish_writing(&mut self, index: usize) {
        let slot_status = self.slots.get_mut(&index).unwrap();
        assert_eq!(slot_status, &SlotStatus::Writing);
        *slot_status = SlotStatus::Finished;

        //Check whether left boundary should move forward
        let mut left_boundary = self.left_boundary;
        while let Some(&SlotStatus::Finished) = self.slots.get(&left_boundary) {
            self.slots.remove(&left_boundary);
            left_boundary += 1;
        }

        self.left_boundary = left_boundary;
    }
}

struct FileWriteCtrl {
    tx_seq: u64,
    total_chunks: usize,
    window: CtrlWindow,
}

impl FileWriteCtrl {
    fn new(tx_seq: u64, total_chunks: usize, window_size: usize) -> Self {
        FileWriteCtrl {
            tx_seq,
            total_chunks,
            window: CtrlWindow::new(window_size),
        }
    }
}

pub struct ChunkPoolWriteCtrl {
    /// Total number of threads that are writing chunks into store.
    pub total_writings: usize,
    /// Windows to control writing processes of files
    files: HashMap<DataRoot, FileWriteCtrl>,
}

impl ChunkPoolWriteCtrl {
    pub fn new() -> Self {
        ChunkPoolWriteCtrl {
            total_writings: 0,
            files: HashMap::default(),
        }
    }

    /// Try to cache the segment into memory pool if log entry not retrieved from blockchain yet.
    /// Otherwise, return segments to write into store asynchronously for different files.
    pub fn write_segment(
        &mut self,
        root: DataRoot,
        tx_seq: u64,
        seg_index: usize,
        file_total_chunk_num: usize,
        write_window_size: usize,
        max_writings: usize,
    ) -> Result<()> {
        let file_ctrl = self
            .files
            .entry(root)
            .or_insert_with(|| FileWriteCtrl::new(tx_seq, file_total_chunk_num, write_window_size));

        if file_ctrl.total_chunks != file_total_chunk_num {
            bail!(anyhow!(
                "file size in segment doesn't match with file size declared in previous segment. Previous total chunk:{}, current total chunk:{}s",
                file_ctrl.total_chunks,
                file_total_chunk_num
            ));
        }

        let window = &mut file_ctrl.window;

        // Segment already uploaded.
        if window.check_duplicate(seg_index) {
            return Ok(());
        }

        // Limits the number of writing threads.
        if self.total_writings >= max_writings {
            bail!(anyhow!("too many data writing: {}", max_writings));
        }

        window.start_writing(seg_index)?;

        self.total_writings += 1;

        Ok(())
    }

    pub fn on_write_succeeded(
        &mut self,
        root: &DataRoot,
        seg_index: usize,
        chunks_per_segment: usize,
        file_total_chunk_num: usize,
    ) -> bool {
        let file_ctrl = match self.files.get_mut(root) {
            Some(w) => w,
            None => return false,
        };

        let window = &mut file_ctrl.window;

        window.finish_writing(seg_index);

        assert!(self.total_writings > 0);
        self.total_writings -= 1;

        debug!("Succeeded to write segment, root={}, seg_index={}, file_total_chunk_num={}, total_writings={}",
            root, seg_index, file_total_chunk_num, self.total_writings);

        // All chunks of file written into store.
        window.left_boundary * chunks_per_segment >= file_total_chunk_num
    }

    pub fn on_write_failed(&mut self, root: &DataRoot, seg_index: usize) {
        let file_ctrl = match self.files.get_mut(root) {
            Some(w) => w,
            None => return,
        };

        let window = &mut file_ctrl.window;

        //Rollback the segment status if failed
        window.rollback_writing(seg_index);

        assert!(self.total_writings > 0);
        self.total_writings -= 1;
    }

    pub fn get_tx_seq(&self, root: &DataRoot) -> u64 {
        let file_ctrl = self.files.get(root).unwrap();
        file_ctrl.tx_seq
    }
}
