use crate::i18n::Language;
use crate::video::WorkerSlot;

#[derive(Clone)]
pub struct CpuTopology {
    pub p_core_masks: Vec<usize>,
    pub e_core_masks: Vec<usize>,
    pub logical_processors: usize,
    pub detected: bool,
    pub fallback_logical_processors: usize,
    pub fallback_worker_limit: usize,
}

impl CpuTopology {
    pub fn detect() -> Self {
        #[cfg(target_os = "windows")]
        {
            if let Some(raw_cores) = win_cpu::processor_core_masks() {
                if !raw_cores.is_empty() {
                    let max_smt = raw_cores
                        .iter()
                        .map(|(_, mask)| mask.count_ones())
                        .max()
                        .unwrap_or(1);

                    let min_smt = raw_cores
                        .iter()
                        .map(|(_, mask)| mask.count_ones())
                        .min()
                        .unwrap_or(1);

                    let max_efficiency = raw_cores
                        .iter()
                        .map(|(efficiency, _)| *efficiency)
                        .max()
                        .unwrap_or(0);

                    let min_efficiency = raw_cores
                        .iter()
                        .map(|(efficiency, _)| *efficiency)
                        .min()
                        .unwrap_or(0);

                    let is_hybrid = (max_efficiency != min_efficiency) || (max_smt != min_smt);

                    let mut p_core_masks = Vec::new();
                    let mut e_core_masks = Vec::new();

                    if is_hybrid {
                        for (efficiency, mask) in &raw_cores {
                            if *efficiency == max_efficiency {
                                p_core_masks.push(*mask);
                            } else {
                                e_core_masks.push(*mask);
                            }
                        }
                    } else {
                        for (_, mask) in &raw_cores {
                            p_core_masks.push(*mask);
                        }
                    }

                    let logical_processors = raw_cores
                        .iter()
                        .map(|(_, mask)| mask.count_ones() as usize)
                        .sum();

                    return Self {
                        p_core_masks,
                        e_core_masks,
                        logical_processors,
                        detected: true,
                        fallback_logical_processors: 0,
                        fallback_worker_limit: 0,
                    };
                }
            }
        }

        let fallback_logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        Self {
            p_core_masks: Vec::new(),
            e_core_masks: Vec::new(),
            logical_processors: 0,
            detected: false,
            fallback_logical_processors: fallback_logical,
            fallback_worker_limit: fallback_logical,
        }
    }

    pub fn is_hybrid(&self) -> bool {
        self.detected && !self.e_core_masks.is_empty()
    }

    pub fn physical_core_count(&self) -> usize {
        if self.detected {
            self.p_core_masks.len() + self.e_core_masks.len()
        } else {
            let fallback = self.fallback_logical_processors;
            (fallback / 2).max(1)
        }
    }

    pub fn recommended_workers(&self) -> usize {
        self.physical_core_count()
    }

    pub fn max_workers_for_mode(&self, core_mode: crate::video::CoreMode) -> usize {
        use crate::video::CoreMode;

        if !self.detected {
            return self.fallback_worker_limit;
        }

        match core_mode {
            CoreMode::POnly => self.p_core_masks.len().max(1),
            CoreMode::EOnly => self.e_core_masks.len().max(1),
            CoreMode::Auto => (self.p_core_masks.len() + self.e_core_masks.len()).max(1),
        }
    }

    pub fn description(&self, lang: Language) -> String {
        self.format_info(lang)
    }

    pub fn pick_worker_slots(&self, max_workers: usize, core_mode: crate::video::CoreMode) -> Vec<WorkerSlot> {
        use crate::video::CoreMode;

        if !self.detected {
            let count = max_workers.min(self.fallback_worker_limit).max(1);
            return vec![WorkerSlot::default(); count];
        }

        let (selected_masks, is_p) = match core_mode {
            CoreMode::Auto | CoreMode::POnly if !self.p_core_masks.is_empty() => {
                (&self.p_core_masks, true)
            }
            CoreMode::EOnly if !self.e_core_masks.is_empty() => (&self.e_core_masks, false),
            CoreMode::Auto if !self.e_core_masks.is_empty() => (&self.e_core_masks, false),
            _ => {
                if !self.p_core_masks.is_empty() {
                    (&self.p_core_masks, true)
                } else if !self.e_core_masks.is_empty() {
                    (&self.e_core_masks, false)
                } else {
                    return vec![WorkerSlot::default(); max_workers.min(4).max(1)];
                }
            }
        };

        let count = max_workers.min(selected_masks.len()).max(1);
        selected_masks
            .iter()
            .take(count)
            .map(|&mask| WorkerSlot {
                affinity_mask: mask,
                is_p_core: is_p,
            })
            .collect()
    }

    pub fn format_info(&self, lang: Language) -> String {
        if self.detected {
            if self.is_hybrid() {
                match lang {
                    Language::Ru => format!(
                        "Обнаружено {} P + {} E ядер ({} логических потоков)",
                        self.p_core_masks.len(),
                        self.e_core_masks.len(),
                        self.logical_processors
                    ),
                    Language::En => format!(
                        "Detected {} P + {} E cores ({} logical threads)",
                        self.p_core_masks.len(),
                        self.e_core_masks.len(),
                        self.logical_processors
                    ),
                }
            } else {
                match lang {
                    Language::Ru => format!(
                        "Обнаружено {} физических ядер ({} логических потоков)",
                        self.physical_core_count(),
                        self.logical_processors
                    ),
                    Language::En => format!(
                        "Detected {} physical cores ({} logical threads)",
                        self.physical_core_count(),
                        self.logical_processors
                    ),
                }
            }
        } else {
            match lang {
                Language::Ru => format!(
                    "Обнаружено {} физических ядер ({} логических потоков)",
                    self.physical_core_count(),
                    self.fallback_logical_processors
                ),
                Language::En => format!(
                    "Detected {} physical cores ({} logical threads)",
                    self.physical_core_count(),
                    self.fallback_logical_processors
                ),
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub mod win_cpu {
    use std::ffi::{c_void, OsStr};
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalProcessorInformationEx(
            relationship_type: u32,
            buffer: *mut c_void,
            returned_length: *mut u32,
        ) -> i32;

        fn GetCurrentThread() -> *mut c_void;
        fn SetThreadAffinityMask(h_thread: *mut c_void, dw_thread_affinity_mask: usize) -> usize;

        fn GlobalAlloc(u_flags: u32, dw_bytes: usize) -> *mut c_void;
        fn GlobalLock(h_mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(h_mem: *mut c_void) -> i32;
        fn GlobalFree(h_mem: *mut c_void) -> *mut c_void;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn OpenClipboard(h_wnd_new_owner: *mut c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn CloseClipboard() -> i32;
        fn SetClipboardData(u_format: u32, h_mem: *mut c_void) -> *mut c_void;
    }

    pub struct ThreadAffinityGuard {
        previous_mask: usize,
    }

    impl ThreadAffinityGuard {
        pub fn pin(mask: usize) -> Self {
            if mask == 0 {
                return Self { previous_mask: 0 };
            }

            let previous_mask = unsafe { SetThreadAffinityMask(GetCurrentThread(), mask) };
            Self { previous_mask }
        }
    }

    impl Drop for ThreadAffinityGuard {
        fn drop(&mut self) {
            if self.previous_mask != 0 {
                unsafe {
                    let _ = SetThreadAffinityMask(GetCurrentThread(), self.previous_mask);
                }
            }
        }
    }

    pub fn set_clipboard_text(text: &str) {
        const CF_UNICODETEXT: u32 = 13;
        const GMEM_MOVEABLE: u32 = 0x0002;

        let wide: Vec<u16> = OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let byte_len = wide.len() * size_of::<u16>();

        unsafe {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                EmptyClipboard();
                let hmem = GlobalAlloc(GMEM_MOVEABLE, byte_len);
                if !hmem.is_null() {
                    let locked = GlobalLock(hmem) as *mut u16;
                    if !locked.is_null() {
                        std::ptr::copy_nonoverlapping(wide.as_ptr(), locked, wide.len());
                        GlobalUnlock(hmem);
                        if SetClipboardData(CF_UNICODETEXT, hmem).is_null() {
                            GlobalFree(hmem);
                        }
                    } else {
                        GlobalFree(hmem);
                    }
                }
                CloseClipboard();
            }
        }
    }

    pub fn processor_core_masks() -> Option<Vec<(u8, usize)>> {
        const RELATION_PROCESSOR_CORE: u32 = 0;

        let mut required_length = 0u32;

        unsafe {
            let _ = GetLogicalProcessorInformationEx(
                RELATION_PROCESSOR_CORE,
                std::ptr::null_mut(),
                &mut required_length,
            );
        }

        if required_length == 0 {
            return None;
        }

        let mut buffer = vec![0u8; required_length as usize];
        let mut returned_length = required_length;

        let success = unsafe {
            GetLogicalProcessorInformationEx(
                RELATION_PROCESSOR_CORE,
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut returned_length,
            )
        };

        if success == 0 {
            return None;
        }

        let valid_length = (returned_length as usize).min(buffer.len());
        buffer.truncate(valid_length);

        let pointer_size = size_of::<usize>();
        let group_affinity_size = pointer_size + 8;

        let mut result = Vec::new();
        let mut offset = 0usize;

        while offset.saturating_add(8) <= buffer.len() {
            let header = buffer.get(offset..offset + 8)?;
            let relationship_type = u32::from_ne_bytes(header[0..4].try_into().ok()?);
            let record_size = u32::from_ne_bytes(header[4..8].try_into().ok()?) as usize;

            if record_size < 8 {
                break;
            }

            let record_end = offset.checked_add(record_size)?;
            if record_end > buffer.len() {
                break;
            }

            if relationship_type == RELATION_PROCESSOR_CORE {
                let body_offset = offset + 8;

                if record_size >= 8 + 24 {
                    let efficiency_class = *buffer.get(body_offset + 1)?;

                    let group_count_bytes = buffer.get(body_offset + 22..body_offset + 24)?;
                    let group_count =
                        u16::from_ne_bytes(group_count_bytes.try_into().ok()?) as usize;

                    let group_mask_offset = body_offset + 24;
                    let mut core_mask = 0usize;

                    for group_index in 0..group_count {
                        let item_offset = group_mask_offset
                            .checked_add(group_index.checked_mul(group_affinity_size)?)?;
                        let item_end = item_offset.checked_add(group_affinity_size)?;

                        if item_end > record_end {
                            break;
                        }

                        let mask_bytes = buffer.get(item_offset..item_offset + pointer_size)?;
                        let mask = usize::from_ne_bytes(mask_bytes.try_into().ok()?);

                        let group_offset = item_offset + pointer_size;
                        let group_bytes = buffer.get(group_offset..group_offset + 2)?;
                        let group = u16::from_ne_bytes(group_bytes.try_into().ok()?);

                        if group == 0 {
                            core_mask |= mask;
                        }
                    }

                    if core_mask != 0 {
                        result.push((efficiency_class, core_mask));
                    }
                }
            }

            offset = record_end;
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }
}

#[cfg(target_os = "windows")]
pub fn pin_current_thread(mask: usize) -> win_cpu::ThreadAffinityGuard {
    win_cpu::ThreadAffinityGuard::pin(mask)
}

#[cfg(not(target_os = "windows"))]
pub struct DummyAffinityGuard;

#[cfg(not(target_os = "windows"))]
pub fn pin_current_thread(_mask: usize) -> DummyAffinityGuard {
    DummyAffinityGuard
}
