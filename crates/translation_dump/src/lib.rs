#![doc = include_str!("../README.md")]

use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::{Res, ResMut, Resource};
use corelib::debug::{DebugHandlerError, MainDebugTopic};
use corelib::hook::{Callback, HookTarget, InstalledHook};
use corelib::il2cpp_recon;
use corelib::{
    AppCtx, CallbackBoundedWriter, CallbackIl2Cpp, Plugin, PluginError, SendOutcome, UpdateCtx,
};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::{panic::AssertUnwindSafe, panic::catch_unwind};

mod targets;
use targets::{
    DataFileGetBytesTarget, GET_BYTES_SITE, GET_TEXT_OR_NULL_SITE, GET_TEXT_SITE,
    GetTextOrNullTarget, GetTextTarget, LiveMvUpdateLyricsTarget, SET_LYRIC_SITE, TMP_TEXT_SITE,
    TimelineSetLyricTarget, TmpTextSetTextTarget, UPDATE_LYRICS_SITE,
};
pub use targets::{
    GetBytesFn, GetTextOrNullFn, Il2CppStringOpaque, LOCALIZATION_GET_TEXT_OR_NULL_TARGET,
    LOCALIZATION_GET_TEXT_TARGET, LyricsFn, MethodInfoOpaque,
};

/// Per-callback payload limits. Oversized values are skipped as a complete
/// record, never silently truncated into invalid dump data.
pub const CATEGORY_UTF16_CAPACITY: usize = 128;
pub const TEXT_UTF16_CAPACITY: usize = 2048;
pub const DUMP_QUEUE_CAPACITY: usize = 256;
pub const PATH_UTF16_CAPACITY: usize = 256;
const FLUSH_INTERVAL_FRAMES: u32 = 60;

type LocalifyMap = BTreeMap<String, BTreeMap<String, String>>;

#[derive(Clone, Copy)]
struct DumpRecord {
    category_len: u16,
    text_len: u16,
    id: i32,
    category: [u16; CATEGORY_UTF16_CAPACITY],
    text: [u16; TEXT_UTF16_CAPACITY],
}

impl Message for DumpRecord {}

impl DumpRecord {
    fn capture(category: &[u16], id: i32, text: &[u16]) -> Option<Self> {
        if category.len() > CATEGORY_UTF16_CAPACITY || text.len() > TEXT_UTF16_CAPACITY {
            return None;
        }
        let mut record = Self {
            category_len: category.len() as u16,
            text_len: text.len() as u16,
            id,
            category: [0; CATEGORY_UTF16_CAPACITY],
            text: [0; TEXT_UTF16_CAPACITY],
        };
        record.category[..category.len()].copy_from_slice(category);
        record.text[..text.len()].copy_from_slice(text);
        Some(record)
    }

    fn decode(&self) -> Result<(String, String, String), ()> {
        let category =
            String::from_utf16(&self.category[..usize::from(self.category_len)]).map_err(|_| ())?;
        let text = String::from_utf16(&self.text[..usize::from(self.text_len)]).map_err(|_| ())?;
        Ok((category, self.id.to_string(), text))
    }
}

/// Relayed lyric/display text with a capture-source tag.
#[derive(Clone, Copy)]
struct LyricsRecord {
    len: u16,
    source: u8,
    text: [u16; TEXT_UTF16_CAPACITY],
}

impl Message for LyricsRecord {}

impl LyricsRecord {
    fn capture(text: &[u16], source: u8) -> Option<Self> {
        if text.len() > TEXT_UTF16_CAPACITY {
            return None;
        }
        let mut record = Self {
            len: text.len() as u16,
            source,
            text: [0; TEXT_UTF16_CAPACITY],
        };
        record.text[..text.len()].copy_from_slice(text);
        Some(record)
    }

    fn decode(&self) -> (u8, String) {
        (
            self.source,
            String::from_utf16(&self.text[..usize::from(self.len)]).unwrap_or_default(),
        )
    }
}

const SRC_MV_OVERLAY: u8 = 0;
const SRC_TIMELINE: u8 = 1;
const SRC_TMP_TEXT: u8 = 2;

#[derive(Default)]
struct DumpDiagnostics {
    get_bytes_seen: AtomicU64,
    get_bytes_dumps: AtomicU64,
    lyrics_seen: AtomicU64,
    lyrics_kept: AtomicU64,
    hook_hits: AtomicU64,
    get_text_hits: AtomicU64,
    enqueued: AtomicU64,
    null_strings: AtomicU64,
    oversized: AtomicU64,
    queue_full: AtomicU64,
    invalid_utf16: AtomicU64,
    merged: AtomicU64,
    flushes: AtomicU64,
}

struct DumpSites {
    il2cpp: CallbackIl2Cpp,
    records: CallbackBoundedWriter<DumpRecord, DUMP_QUEUE_CAPACITY>,
    json_dir: PathBuf,
    lyrics: CallbackBoundedWriter<LyricsRecord, DUMP_QUEUE_CAPACITY>,
    diagnostics: Arc<DumpDiagnostics>,
    /// Manager instance captured from the first dump-hook invocation
    /// (`this` of `GetText`/`GetTextOrNull`); 0 until then.
    instance: AtomicUsize,
}

#[derive(Resource)]
struct DumpHook {
    _hook: InstalledHook<GetTextOrNullTarget, DumpSites>,
    /// `None` when the `GetText` install was rejected (signature drift after
    /// a game update); the `GetTextOrNull` hook keeps working either way.
    _get_text_hook: Option<InstalledHook<GetTextTarget, DumpSites>>,
    _get_bytes_hook: Option<InstalledHook<DataFileGetBytesTarget, DumpSites>>,
    _update_lyrics_hook: Option<InstalledHook<LiveMvUpdateLyricsTarget, DumpSites>>,
    _set_lyric_hook: Option<InstalledHook<TimelineSetLyricTarget, DumpSites>>,
    _tmp_text_hook: Option<InstalledHook<TmpTextSetTextTarget, DumpSites>>,
}

/// Main-domain handle onto the shared dump container, for debug topics.
#[derive(Resource)]
struct DumpSitesResource(Arc<DumpSites>);

#[derive(Resource)]
struct DumpState {
    path: PathBuf,
    entries: LocalifyMap,
    dirty: bool,
    frames_since_flush: u32,
    diagnostics: Arc<DumpDiagnostics>,
    /// Class-3 passive capture state (main domain only).
    lyrics: BTreeMap<String, String>,
    lyrics_dirty: bool,
    /// Probe cursor into the sorted mlADVInfo_Title sid list.
    scenario_cursor: usize,
}

impl DumpState {
    fn load(path: PathBuf, diagnostics: Arc<DumpDiagnostics>) -> Result<Self, PluginError> {
        let entries = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| PluginError::Message("translation dump JSON is invalid"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LocalifyMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            entries,
            dirty: false,
            frames_since_flush: 0,
            diagnostics,
            lyrics: BTreeMap::new(),
            lyrics_dirty: false,
            scenario_cursor: 0,
        })
    }

    fn merge(&mut self, record: &DumpRecord) {
        let Ok((category, id, text)) = record.decode() else {
            self.diagnostics
                .invalid_utf16
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        let old = self
            .entries
            .entry(category)
            .or_default()
            .insert(id, text.clone());
        if old.as_deref() != Some(text.as_str()) {
            self.dirty = true;
        }
        self.diagnostics.merged.fetch_add(1, Ordering::Relaxed);
    }

    /// Merge a whole-table map (e.g. the serialized `dic`) into the dump,
    /// reporting how many categories/entries it contributed.
    fn merge_map(&mut self, map: &LocalifyMap) -> (usize, usize) {
        let mut categories = 0;
        let mut entries = 0;
        for (category, bucket) in map {
            categories += 1;
            let slot = self.entries.entry(category.clone()).or_default();
            for (id, text) in bucket {
                entries += 1;
                let old = slot.insert(id.clone(), text.clone());
                if old.as_deref() != Some(text.as_str()) {
                    self.dirty = true;
                }
                self.diagnostics.merged.fetch_add(1, Ordering::Relaxed);
            }
        }
        (categories, entries)
    }

    fn flush_if_due(&mut self) -> Result<(), PluginError> {
        self.frames_since_flush = self.frames_since_flush.saturating_add(1);
        if self.dirty && self.frames_since_flush >= FLUSH_INTERVAL_FRAMES {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<bool, PluginError> {
        if !self.dirty && !self.lyrics_dirty {
            self.frames_since_flush = 0;
            return Ok(false);
        }
        write_atomic_json(&self.path, &self.entries)?;
        self.dirty = false;
        if self.lyrics_dirty {
            let lyrics_path = self
                .path
                .parent()
                .unwrap_or(Path::new("."))
                .join("lyrics_dump.json");
            let lyrics_tmp = lyrics_path.with_extension("json.tmp");
            if let Ok(file) = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&lyrics_tmp)
            {
                if serde_json::to_writer_pretty(&mut std::io::BufWriter::new(file), &self.lyrics)
                    .is_ok()
                {
                    let _ = std::fs::rename(&lyrics_tmp, &lyrics_path);
                }
                let _ = std::fs::remove_file(&lyrics_tmp);
            }
            self.lyrics_dirty = false;
        }
        self.frames_since_flush = 0;
        self.diagnostics.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }
}

/// Shared capture body of both dump replacements: calls the original exactly
/// once, copies (category, id, text) into a bounded record, and returns the
/// original result unchanged.
fn capture_text<T>(
    callback: &Callback<'_, T, DumpSites>,
    this: *mut c_void,
    category: *mut Il2CppStringOpaque,
    id: i32,
    value: *mut Il2CppStringOpaque,
    returned: &Cell<*mut Il2CppStringOpaque>,
    hits: &AtomicU64,
) -> *mut Il2CppStringOpaque
where
    T: HookTarget<Original = GetTextOrNullFn>,
{
    returned.set(value);
    let sites = callback.container();
    hits.fetch_add(1, Ordering::Relaxed);
    // Remember the manager instance for the whole-table `dicdump` topic.
    sites.instance.store(this as usize, Ordering::Release);
    // SAFETY: both pointers are live managed strings owned by this native
    // method invocation and borrowed only here.
    let category_utf16 = unsafe {
        sites
            .il2cpp
            .string_utf16(callback.cap(), category.cast::<c_void>())
    };
    // SAFETY: the returned string remains live through this hook callback
    // and the copied code units do not escape by borrow.
    let text_utf16 = unsafe {
        sites
            .il2cpp
            .string_utf16(callback.cap(), value.cast::<c_void>())
    };
    match (category_utf16, text_utf16) {
        (Some(category_utf16), Some(text_utf16)) => {
            match DumpRecord::capture(category_utf16, id, text_utf16) {
                Some(record) => match sites.records.try_send(callback.cap(), record) {
                    SendOutcome::Accepted => {
                        sites.diagnostics.enqueued.fetch_add(1, Ordering::Relaxed);
                    }
                    SendOutcome::Full => {
                        sites.diagnostics.queue_full.fetch_add(1, Ordering::Relaxed);
                    }
                    SendOutcome::Replaced | SendOutcome::Busy => {}
                },
                None => {
                    sites.diagnostics.oversized.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        _ => {
            sites
                .diagnostics
                .null_strings
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    value
}

/// Capture the original localization result without changing it
/// (`GetTextOrNull` entry).
unsafe extern "C" fn get_text_or_null_replacement(
    this: *mut c_void,
    category: *mut Il2CppStringOpaque,
    id: i32,
    method: *const MethodInfoOpaque,
) -> *mut Il2CppStringOpaque {
    let returned = Cell::new(core::ptr::null_mut());
    catch_unwind(AssertUnwindSafe(|| {
        GET_TEXT_OR_NULL_SITE.dispatch(
            |original| {
                // SAFETY: target validation binds this exact instance ABI.
                let value = unsafe { original(this, category, id, method) };
                returned.set(value);
                value
            },
            || returned.get(),
            |callback| {
                let value = callback
                    .call_original(|original| {
                        // SAFETY: same target-bound ABI as the passthrough.
                        unsafe { original(this, category, id, method) }
                    })
                    .unwrap_or_else(|| returned.get());
                let sites = callback.container();
                capture_text(
                    callback,
                    this,
                    category,
                    id,
                    value,
                    &returned,
                    &sites.diagnostics.hook_hits,
                )
            },
        )
    }))
    .unwrap_or_else(|_| returned.get())
}

/// Capture the original localization result without changing it
/// (`GetText` entry — the live UI text path per runtime recon).
unsafe extern "C" fn get_text_replacement(
    this: *mut c_void,
    category: *mut Il2CppStringOpaque,
    id: i32,
    method: *const MethodInfoOpaque,
) -> *mut Il2CppStringOpaque {
    let returned = Cell::new(core::ptr::null_mut());
    catch_unwind(AssertUnwindSafe(|| {
        GET_TEXT_SITE.dispatch(
            |original| {
                // SAFETY: target validation binds this exact instance ABI.
                let value = unsafe { original(this, category, id, method) };
                returned.set(value);
                value
            },
            || returned.get(),
            |callback| {
                let value = callback
                    .call_original(|original| {
                        // SAFETY: same target-bound ABI as the passthrough.
                        unsafe { original(this, category, id, method) }
                    })
                    .unwrap_or_else(|| returned.get());
                let sites = callback.container();
                capture_text(
                    callback,
                    this,
                    category,
                    id,
                    value,
                    &returned,
                    &sites.diagnostics.get_text_hits,
                )
            },
        )
    }))
    .unwrap_or_else(|_| returned.get())
}

/// Passive class-3 discovery: relay `DataFile.GetBytes` paths to the main
/// thread (`byte[]` itself is re-read on the main thread; no I/O in hooks).
unsafe extern "C" fn get_bytes_replacement(
    path: *mut Il2CppStringOpaque,
    method: *const MethodInfoOpaque,
) -> *mut c_void {
    let returned = Cell::new(core::ptr::null_mut());
    catch_unwind(AssertUnwindSafe(|| {
        GET_BYTES_SITE.dispatch(
            |original| {
                // SAFETY: target validation binds this exact static ABI.
                let value = unsafe { original(path, method) };
                returned.set(value);
                value
            },
            || returned.get(),
            |callback| {
                let value = callback
                    .call_original(|original| unsafe { original(path, method) })
                    .unwrap_or_else(|| returned.get());
                returned.set(value);
                let sites = callback.container();
                sites
                    .diagnostics
                    .get_bytes_seen
                    .fetch_add(1, Ordering::Relaxed);
                // SAFETY: `path` is a live managed string owned by this call.
                let Some(path_units) = (unsafe {
                    sites
                        .il2cpp
                        .string_utf16(callback.cap(), path.cast::<c_void>())
                }) else {
                    return value;
                };
                let path_str = String::from_utf16_lossy(path_units);
                if !path_str.ends_with(".json") || path_str.contains("..") {
                    return value;
                }
                // SAFETY: `value` is a live IL2CPP byte[] object: max_length
                // at +0x18, element data at +0x20.
                let len = unsafe { std::ptr::read_volatile((value as usize + 0x18) as *const u32) }
                    as usize;
                if len == 0 || len > MAX_DATA_FILE_BYTES {
                    return value;
                }
                let mut bytes = vec![0u8; len];
                // SAFETY: [value + 0x20, value + 0x20 + len) is inside the
                // array object, live for the duration of this call.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        (value as usize + 0x20) as *const u8,
                        bytes.as_mut_ptr(),
                        len,
                    );
                }
                let sanitized: String = path_str
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                if sanitized.contains("..") {
                    return value;
                }
                let target = sites.json_dir.join(&sanitized);
                if !target.exists() {
                    let _ = std::fs::write(&target, &bytes);
                    sites
                        .diagnostics
                        .get_bytes_dumps
                        .fetch_add(1, Ordering::Relaxed);
                }
                value
            },
        )
    }))
    .unwrap_or_else(|_| returned.get())
}

/// Lyric line capture (MV overlay path) — original call is returned unchanged.
unsafe extern "C" fn update_lyrics_replacement(
    this: *mut c_void,
    text: *mut Il2CppStringOpaque,
    method: *const MethodInfoOpaque,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        UPDATE_LYRICS_SITE.dispatch(
            |original| {
                // SAFETY: target validation binds this exact instance ABI.
                unsafe { original(this, text, method) }
            },
            || (),
            |callback| {
                let sites = callback.container();
                sites
                    .diagnostics
                    .lyrics_seen
                    .fetch_add(1, Ordering::Relaxed);
                // SAFETY: `text` is a live managed string owned by this call.
                if let Some(units) = unsafe {
                    sites
                        .il2cpp
                        .string_utf16(callback.cap(), text.cast::<c_void>())
                } && let Some(record) = LyricsRecord::capture(units, SRC_MV_OVERLAY)
                {
                    match sites.lyrics.try_send(callback.cap(), record) {
                        SendOutcome::Accepted => {
                            sites
                                .diagnostics
                                .lyrics_kept
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            sites.diagnostics.queue_full.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            },
        )
    }));
}

/// Lyric line capture (timeline path) — original call is returned unchanged.
unsafe extern "C" fn set_lyric_replacement(
    this: *mut c_void,
    text: *mut Il2CppStringOpaque,
    method: *const MethodInfoOpaque,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        SET_LYRIC_SITE.dispatch(
            |original| {
                // SAFETY: target validation binds this exact instance ABI.
                unsafe { original(this, text, method) }
            },
            || (),
            |callback| {
                let sites = callback.container();
                sites
                    .diagnostics
                    .lyrics_seen
                    .fetch_add(1, Ordering::Relaxed);
                // SAFETY: `text` is a live managed string owned by this call.
                if let Some(units) = unsafe {
                    sites
                        .il2cpp
                        .string_utf16(callback.cap(), text.cast::<c_void>())
                } && let Some(record) = LyricsRecord::capture(units, SRC_TIMELINE)
                {
                    match sites.lyrics.try_send(callback.cap(), record) {
                        SendOutcome::Accepted => {
                            sites
                                .diagnostics
                                .lyrics_kept
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            sites.diagnostics.queue_full.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            },
        )
    }));
}

/// Catch-all display-text capture (`TMP_Text.set_text`): high volume, the
/// main side deduplicates by text and records the capture source.
unsafe extern "C" fn tmp_text_replacement(
    this: *mut c_void,
    text: *mut Il2CppStringOpaque,
    method: *const MethodInfoOpaque,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        TMP_TEXT_SITE.dispatch(
            |original| {
                // SAFETY: target validation binds this exact instance ABI.
                unsafe { original(this, text, method) }
            },
            || (),
            |callback| {
                let sites = callback.container();
                sites
                    .diagnostics
                    .lyrics_seen
                    .fetch_add(1, Ordering::Relaxed);
                // SAFETY: `text` is a live managed string owned by this call.
                if let Some(units) = unsafe {
                    sites
                        .il2cpp
                        .string_utf16(callback.cap(), text.cast::<c_void>())
                } && let Some(record) = LyricsRecord::capture(units, SRC_TMP_TEXT)
                {
                    match sites.lyrics.try_send(callback.cap(), record) {
                        SendOutcome::Accepted => {
                            sites
                                .diagnostics
                                .lyrics_kept
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            sites.diagnostics.queue_full.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            },
        )
    }));
}

/// Development-only localization dump plugin.
pub struct TranslationDumpPlugin;

impl Plugin for TranslationDumpPlugin {
    fn name(&self) -> &'static str {
        "translation_dump"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        let diagnostics = Arc::new(DumpDiagnostics::default());
        let path = ctx
            .data_root()
            .join("shiny-song-tools")
            .join("dumps")
            .join("localify.json");
        let path_display = path.display().to_string();
        let state = DumpState::load(path, Arc::clone(&diagnostics))?;
        ctx.insert_resource(state)?;

        let (records, _main_reader) =
            ctx.callback_to_main_bounded::<DumpRecord, DUMP_QUEUE_CAPACITY>()?;
        let (lyrics, _lyrics_reader) =
            ctx.callback_to_main_bounded::<LyricsRecord, DUMP_QUEUE_CAPACITY>()?;
        let il2cpp = ctx.callback_il2cpp();
        let json_dir = ctx
            .data_root()
            .join("shiny-song-tools")
            .join("dumps")
            .join("json");
        let _ = std::fs::create_dir_all(&json_dir);
        let sites = ctx.register_container(DumpSites {
            il2cpp,
            records,
            json_dir,
            lyrics,
            diagnostics,
            instance: AtomicUsize::new(0),
        })?;
        ctx.insert_resource(DumpSitesResource(Arc::clone(&sites)))?;
        let hook = ctx
            .hook(&GET_TEXT_OR_NULL_SITE)
            .container(sites.clone())
            .handler(get_text_or_null_replacement as GetTextOrNullFn)?
            .install()?;
        // The `GetText` overload is the live UI text path (896 direct call
        // sites at recon time) but its signature may drift between game
        // updates; a rejected install is logged and skipped rather than
        // failing the whole dump plugin.
        let get_text_hook = match ctx
            .hook(&GET_TEXT_SITE)
            .container(sites.clone())
            .handler(get_text_replacement as GetTextOrNullFn)?
            .install()
        {
            Ok(hook) => Some(hook),
            Err(error) => {
                tracing::warn!(
                    target: "translation_dump",
                    %error,
                    "GetText hook skipped; falling back to GetTextOrNull only"
                );
                None
            }
        };
        // Class-3 discovery + lyrics capture hooks: same resilience — a
        // rejected install (signature drift) is logged and skipped.
        let get_bytes_hook = match ctx
            .hook(&GET_BYTES_SITE)
            .container(sites.clone())
            .handler(get_bytes_replacement as GetBytesFn)?
            .install()
        {
            Ok(hook) => Some(hook),
            Err(error) => {
                tracing::warn!(target: "translation_dump", %error, "DataFile.GetBytes hook skipped");
                None
            }
        };
        let update_lyrics_hook = match ctx
            .hook(&UPDATE_LYRICS_SITE)
            .container(sites.clone())
            .handler(update_lyrics_replacement as LyricsFn)?
            .install()
        {
            Ok(hook) => Some(hook),
            Err(error) => {
                tracing::warn!(target: "translation_dump", %error, "UpdateLyrics hook skipped");
                None
            }
        };
        let set_lyric_hook = match ctx
            .hook(&SET_LYRIC_SITE)
            .container(sites.clone())
            .handler(set_lyric_replacement as LyricsFn)?
            .install()
        {
            Ok(hook) => Some(hook),
            Err(error) => {
                tracing::warn!(target: "translation_dump", %error, "SetLyric hook skipped");
                None
            }
        };
        let tmp_text_hook = match ctx
            .hook(&TMP_TEXT_SITE)
            .container(sites.clone())
            .handler(tmp_text_replacement as LyricsFn)?
            .install()
        {
            Ok(hook) => Some(hook),
            Err(error) => {
                tracing::warn!(target: "translation_dump", %error, "TMP_Text hook skipped");
                None
            }
        };
        ctx.insert_resource(DumpHook {
            _hook: hook,
            _get_text_hook: get_text_hook,
            _get_bytes_hook: get_bytes_hook,
            _update_lyrics_hook: update_lyrics_hook,
            _set_lyric_hook: set_lyric_hook,
            _tmp_text_hook: tmp_text_hook,
        })?;

        ctx.add_update_system(dump_update);
        ctx.register_main_debug::<fn(Res<'static, DumpState>), DumpStatus, _>(dump_status_handler)?;
        ctx.register_main_debug::<fn(ResMut<'static, DumpState>), DumpFlush, _>(
            dump_flush_handler,
        )?;
        ctx.register_main_debug::<fn(Res<'static, DumpSitesResource>, ResMut<'static, DumpState>), DumpDic, _>(
            dump_dic_handler,
        )?;
        ctx.register_main_debug::<fn(ResMut<'static, DumpState>), ScenarioDump, _>(
            scenario_dump_handler,
        )?;
        tracing::info!(
            target: "translation_dump",
            path = %path_display,
            "translation dump plugin registered"
        );
        Ok(())
    }
}

const MAX_DATA_FILE_BYTES: usize = 64 * 1024 * 1024;

fn dump_update(
    _ctx: UpdateCtx<'_>,
    mut state: ResMut<DumpState>,
    mut records: MessageReader<DumpRecord>,
    mut lyrics: MessageReader<LyricsRecord>,
) -> Result<(), PluginError> {
    for record in records.read() {
        state.merge(record);
    }
    for record in lyrics.read() {
        let (source, text) = record.decode();
        if text.trim().is_empty() {
            continue;
        }
        if state.lyrics.insert(text.clone(), String::new()).is_none() {
            state.lyrics_dirty = true;
        }
        let _ = source;
    }
    state.flush_if_due()
}

pub struct DumpStatus;
impl MainDebugTopic for DumpStatus {
    const NAME: &'static str = "translation_dump.status";
    type Request = DumpStatusRequest;
    type Response = DumpStatusResponse;
}

#[derive(serde::Deserialize)]
pub struct DumpStatusRequest {}

#[derive(serde::Serialize)]
pub struct DumpStatusResponse {
    pub categories: usize,
    pub entries: usize,
    pub dirty: bool,
    pub hook_hits: u64,
    pub get_text_hits: u64,
    pub get_bytes_seen: u64,
    pub get_bytes_dumps: u64,
    pub lyrics_seen: u64,
    pub lyrics_kept: u64,
    pub enqueued: u64,
    pub null_strings: u64,
    pub oversized: u64,
    pub queue_full: u64,
    pub invalid_utf16: u64,
    pub merged: u64,
    pub flushes: u64,
}

fn dump_status_handler(
    _ctx: UpdateCtx<'_>,
    _request: DumpStatusRequest,
    state: Res<DumpState>,
) -> Result<DumpStatusResponse, DebugHandlerError> {
    let diagnostics = &state.diagnostics;
    Ok(DumpStatusResponse {
        categories: state.entries.len(),
        entries: state.entries.values().map(BTreeMap::len).sum(),
        dirty: state.dirty,
        hook_hits: diagnostics.hook_hits.load(Ordering::Relaxed),
        get_text_hits: diagnostics.get_text_hits.load(Ordering::Relaxed),
        get_bytes_seen: diagnostics.get_bytes_seen.load(Ordering::Relaxed),
        get_bytes_dumps: diagnostics.get_bytes_dumps.load(Ordering::Relaxed),
        lyrics_seen: diagnostics.lyrics_seen.load(Ordering::Relaxed),
        lyrics_kept: diagnostics.lyrics_kept.load(Ordering::Relaxed),
        enqueued: diagnostics.enqueued.load(Ordering::Relaxed),
        null_strings: diagnostics.null_strings.load(Ordering::Relaxed),
        oversized: diagnostics.oversized.load(Ordering::Relaxed),
        queue_full: diagnostics.queue_full.load(Ordering::Relaxed),
        invalid_utf16: diagnostics.invalid_utf16.load(Ordering::Relaxed),
        merged: diagnostics.merged.load(Ordering::Relaxed),
        flushes: diagnostics.flushes.load(Ordering::Relaxed),
    })
}

pub struct DumpFlush;
impl MainDebugTopic for DumpFlush {
    const NAME: &'static str = "translation_dump.flush";
    type Request = DumpFlushRequest;
    type Response = DumpFlushResponse;
}

#[derive(serde::Deserialize)]
pub struct DumpFlushRequest {}

#[derive(serde::Serialize)]
pub struct DumpFlushResponse {
    pub flushed: bool,
}

fn dump_flush_handler(
    _ctx: UpdateCtx<'_>,
    _request: DumpFlushRequest,
    mut state: ResMut<DumpState>,
) -> Result<DumpFlushResponse, DebugHandlerError> {
    let flushed = state
        .flush()
        .map_err(|error| DebugHandlerError(error.to_string()))?;
    Ok(DumpFlushResponse { flushed })
}

pub struct DumpDic;
impl MainDebugTopic for DumpDic {
    const NAME: &'static str = "translation_dump.dicdump";
    type Request = DumpDicRequest;
    type Response = Result<DumpDicResponse, String>;
}

#[derive(serde::Deserialize)]
pub struct DumpDicRequest {}

#[derive(serde::Serialize)]
pub struct DumpDicResponse {
    pub instance: usize,
    pub merged_categories: usize,
    pub merged_entries: usize,
    pub flushed: bool,
}

/// Whole-table dump: serialize `LocalizationManager.dic` with the game's own
/// Newtonsoft and merge it into `localify.json`. Requires the manager
/// instance to have been captured by a prior dump-hook hit.
#[allow(clippy::too_many_lines)]
fn dump_dic_handler(
    _ctx: UpdateCtx<'_>,
    _request: DumpDicRequest,
    sites: Res<DumpSitesResource>,
    mut state: ResMut<DumpState>,
) -> Result<Result<DumpDicResponse, String>, DebugHandlerError> {
    let inner = move || -> Result<DumpDicResponse, String> {
        let instance = sites.0.instance.load(Ordering::Acquire);
        if instance == 0 {
            return Err("manager instance not captured yet; visit a screen with text first".into());
        }
        let Ok(surface) = il2cpp_recon::class_surface(
            "PRISM.Legacy.dll",
            "ENTERPRISE.Localization.LocalizationManager",
        ) else {
            return Err("LocalizationManager surface unavailable".into());
        };
        let Some(dic_offset) = surface
            .fields
            .iter()
            .find(|field| field.name == "dic")
            .map(|field| field.offset as usize)
        else {
            return Err("dic field missing on LocalizationManager".into());
        };
        // SAFETY: `instance` is a live managed object captured from a hook
        // call; the dic field offset comes from live metadata.
        let dic_ptr = unsafe { std::ptr::read_volatile((instance + dic_offset) as *const usize) };
        if dic_ptr == 0 {
            return Err("dic is null".into());
        }
        // Walk the Dictionary<string, Dictionary<int, string>> manually:
        // Newtonsoft's SerializeObject is stripped from the iOS build.
        let mut parsed = LocalifyMap::new();
        walk_string_dictionary(dic_ptr, &mut parsed)?;
        let (merged_categories, merged_entries) = state.merge_map(&parsed);
        let flushed = state.flush().map_err(|error| error.to_string())?;
        Ok(DumpDicResponse {
            instance,
            merged_categories,
            merged_entries,
            flushed,
        })
    };
    let mut inner = inner;
    Ok(inner())
}

/// Standard 64-bit IL2CPP object/array/dictionary layout offsets, validated
/// at runtime by the sanity checks below (malformed shapes abort the walk
/// with an error instead of trusting the guess).
const IL2CPP_ARRAY_DATA: usize = 0x20;
const DICT_ENTRIES: usize = 0x18;
const DICT_COUNT: usize = 0x20;
/// `Dictionary Entry { int hashCode; int next; TKey key; TValue value; }`:
/// 0x18 bytes for (string→ref) and (int→ref) shapes alike.
const ENTRY_STRIDE: usize = 0x18;
const ENTRY_HASH: usize = 0x0;
const ENTRY_KEY: usize = 0x8;
const ENTRY_VALUE: usize = 0x10;
const MAX_WALK_ENTRIES: usize = 500_000;

unsafe fn read_u32(ptr: usize) -> Result<u32, String> {
    // SAFETY: pointers come from live managed objects validated by the walk.
    Ok(unsafe { std::ptr::read_volatile(ptr as *const u32) })
}

unsafe fn read_ptr(ptr: usize) -> Result<usize, String> {
    // SAFETY: pointer field of a live managed object.
    Ok(unsafe { std::ptr::read_volatile(ptr as *const usize) })
}

unsafe fn read_string(ptr: usize) -> Result<String, String> {
    // SAFETY: `ptr` is the key/value field of a live dictionary entry,
    // pointing to a live IL2CPP string (or null).
    let units = unsafe { il2cpp_recon::read_il2cpp_string_utf16(ptr) }
        .ok_or("null string in dictionary entry")?;
    String::from_utf16(&units).map_err(|_| "invalid utf16 in dictionary string".into())
}

/// Walk `Dictionary<string, Dictionary<int, string>>` via the standard IL2CPP
/// layout, collecting category → (id → text). Sanity checks abort on any
/// shape that does not look like a live dictionary.
fn walk_string_dictionary(dict: usize, out: &mut LocalifyMap) -> Result<(), String> {
    if dict == 0 {
        return Err("outer dictionary is null".into());
    }
    // SAFETY: `dict` is a live managed Dictionary captured from the manager.
    let count = unsafe { read_u32(dict + DICT_COUNT)? } as usize;
    let entries = unsafe { read_ptr(dict + DICT_ENTRIES)? };
    if entries == 0 || count > MAX_WALK_ENTRIES {
        return Err(format!("outer dictionary malformed: count={count}"));
    }
    for index in 0..count {
        let entry = entries + IL2CPP_ARRAY_DATA + index * ENTRY_STRIDE;
        // SAFETY: inside the entries array bounds validated via count.
        let hash = unsafe { read_u32(entry + ENTRY_HASH)? } as i32;
        if hash < 0 {
            continue; // freed slot
        }
        // SAFETY: key/value fields of a live entry inside the array.
        let key = unsafe { read_ptr(entry + ENTRY_KEY)? };
        let value = unsafe { read_ptr(entry + ENTRY_VALUE)? };
        let category = unsafe { read_string(key)? };
        if category.is_empty() {
            continue;
        }
        let bucket = out.entry(category).or_default();
        walk_int_dictionary(value, bucket)?;
    }
    Ok(())
}

/// Walk `Dictionary<int, string>` into (id → text).
fn walk_int_dictionary(dict: usize, out: &mut BTreeMap<String, String>) -> Result<(), String> {
    if dict == 0 {
        return Ok(()); // empty inner dictionary is legal
    }
    // SAFETY: `dict` is the live value of an outer dictionary entry.
    let count = unsafe { read_u32(dict + DICT_COUNT)? } as usize;
    let entries = unsafe { read_ptr(dict + DICT_ENTRIES)? };
    if entries == 0 {
        return Ok(());
    }
    if count > MAX_WALK_ENTRIES {
        return Err(format!("inner dictionary malformed: count={count}"));
    }
    for index in 0..count {
        let entry = entries + IL2CPP_ARRAY_DATA + index * ENTRY_STRIDE;
        // SAFETY: inside the inner entries array bounds.
        let hash = unsafe { read_u32(entry + ENTRY_HASH)? } as i32;
        if hash < 0 {
            continue;
        }
        // SAFETY: key/value fields of a live inner entry.
        let id = unsafe { read_u32(entry + ENTRY_KEY)? };
        let value = unsafe { read_ptr(entry + ENTRY_VALUE)? };
        let text = unsafe { read_string(value)? };
        out.insert(id.to_string(), text);
    }
    Ok(())
}

/// Re-invoke `DataFile.GetBytes(key)` on the main thread and write the
/// returned `byte[]` to `target`.
fn dump_one_data_file(va: usize, key: &str, target: &Path) -> Result<(), String> {
    let units: Vec<u16> = key.encode_utf16().collect();
    // SAFETY: main thread is attached; the API allocates a new string.
    let key_ptr = unsafe { il2cpp_recon::new_string_utf16(&units) };
    if key_ptr == 0 {
        return Err("string_new failed".into());
    }
    // SAFETY: `va` is the compiled static entry of DataFile.GetBytes; the
    // key is a live string; the trailing MethodInfo argument is unused.
    let get_bytes: unsafe extern "C" fn(*const c_void, *const c_void) -> *mut c_void =
        unsafe { core::mem::transmute(va) };
    let array = unsafe { get_bytes(key_ptr as *const c_void, std::ptr::null()) } as usize;
    if array == 0 {
        return Err("GetBytes returned null".into());
    }
    // SAFETY: `array` is a live IL2CPP byte[] object: max_length at +0x18,
    // element data at +0x20.
    let len = unsafe { std::ptr::read_volatile((array + 0x18) as *const u32) } as usize;
    if len > MAX_DATA_FILE_BYTES {
        return Err(format!("data file too large: {len}"));
    }
    let mut bytes = vec![0u8; len];
    // SAFETY: [array + 0x20, array + 0x20 + len) is inside the array object.
    unsafe {
        std::ptr::copy_nonoverlapping((array + 0x20) as *const u8, bytes.as_mut_ptr(), len);
    }
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(target, &bytes).map_err(|error| error.to_string())
}

pub struct ScenarioDump;
impl MainDebugTopic for ScenarioDump {
    const NAME: &'static str = "translation_dump.scenariodump";
    type Request = ScenarioDumpRequest;
    type Response = Result<ScenarioDumpResponse, String>;
}

#[derive(serde::Deserialize)]
pub struct ScenarioDumpRequest {
    /// How many scenario ids to process this call (default 25). The probe
    /// runs synchronously on the main thread: the game freezes for the
    /// duration, so keep batches small.
    pub limit: Option<usize>,
}

#[derive(serde::Serialize)]
pub struct ScenarioDumpResponse {
    pub total_sids: usize,
    pub cursor: usize,
    pub sids_done: usize,
    pub files_written: usize,
    pub done: bool,
}

/// Proactive class-3 bulk dump: derive scenario ids from the dic's
/// `mlADVInfo_Title_*` keys, probe `{sid}_{NN}.json` via the game's own
/// `DataFile` API and write every file found. Chunked: repeat the call until
/// `done`.
fn scenario_dump_handler(
    _ctx: UpdateCtx<'_>,
    request: ScenarioDumpRequest,
    mut state: ResMut<DumpState>,
) -> Result<Result<ScenarioDumpResponse, String>, DebugHandlerError> {
    let inner = move || -> Result<ScenarioDumpResponse, String> {
        let total_sids = state
            .entries
            .keys()
            .filter(|k| k.starts_with("mlADVInfo_Title_"))
            .count();
        let mut sids: Vec<String> = state
            .entries
            .keys()
            .filter(|k| k.starts_with("mlADVInfo_Title_"))
            .map(|k| k.trim_start_matches("mlADVInfo_Title_").to_owned())
            .collect();
        sids.sort();
        sids.dedup();
        let is_key_va =
            il2cpp_recon::resolve_method_va("PRISM.Legacy.dll", "PRISM.DataFile", "IsKeyExist", 1)
                .map_err(|error| error.to_string())?
                .ok_or("DataFile.IsKeyExist(1) not found")?;
        let get_bytes_va =
            il2cpp_recon::resolve_method_va("PRISM.Legacy.dll", "PRISM.DataFile", "GetBytes", 1)
                .map_err(|error| error.to_string())?
                .ok_or("DataFile.GetBytes(1) not found")?;
        let limit = request.limit.unwrap_or(25);
        let json_dir = state.path.parent().unwrap_or(Path::new(".")).join("json");
        let _ = std::fs::create_dir_all(&json_dir);

        let mut sids_done = 0usize;
        let mut files_written = 0usize;
        while state.scenario_cursor < sids.len() && sids_done < limit {
            let sid = &sids[state.scenario_cursor];
            let mut part = 0u32;
            let mut misses = 0u32;
            while misses < 5 {
                let name = format!("{sid}_{part:02}.json");
                // SAFETY: main thread attached; allocates a managed string.
                let key = unsafe {
                    il2cpp_recon::new_string_utf16(&name.encode_utf16().collect::<Vec<_>>())
                };
                if key == 0 {
                    return Err("string_new failed".into());
                }
                // SAFETY: compiled static entries of DataFile.IsKeyExist /
                // GetBytes; trailing MethodInfo argument unused.
                let exists = unsafe {
                    let f: unsafe extern "C" fn(*const c_void, *const c_void) -> bool =
                        core::mem::transmute(is_key_va);
                    f(key as *const c_void, std::ptr::null())
                };
                if !exists {
                    misses += 1;
                    part += 1;
                    continue;
                }
                misses = 0;
                dump_one_data_file(get_bytes_va as usize, &name, &json_dir.join(&name))?;
                files_written += 1;
                part += 1;
            }
            sids_done += 1;
            state.scenario_cursor += 1;
        }
        Ok(ScenarioDumpResponse {
            total_sids,
            cursor: state.scenario_cursor,
            sids_done,
            files_written,
            done: state.scenario_cursor >= sids.len(),
        })
    };
    let mut inner = inner;
    Ok(inner())
}

fn write_atomic_json(path: &Path, entries: &LocalifyMap) -> Result<(), PluginError> {
    let parent = path
        .parent()
        .ok_or(PluginError::Message("translation dump path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temp = path.with_extension("json.tmp");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, entries)
        .map_err(|_| PluginError::Message("translation dump JSON encode failed"))?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    let file = writer
        .into_inner()
        .map_err(|error| PluginError::Io(error.into_error()))?;
    file.sync_all()?;
    std::fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(test: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shiny-song-tools-translation-dump-{}-{}",
            std::process::id(),
            test
        ))
    }

    #[test]
    fn production_target_declares_the_complete_signature() {
        assert_eq!(
            LOCALIZATION_GET_TEXT_OR_NULL_TARGET.assembly,
            "PRISM.Legacy.dll"
        );
        assert_eq!(
            LOCALIZATION_GET_TEXT_OR_NULL_TARGET.namespace,
            "ENTERPRISE.Localization"
        );
        assert_eq!(
            LOCALIZATION_GET_TEXT_OR_NULL_TARGET.class,
            "LocalizationManager"
        );
        assert_eq!(LOCALIZATION_GET_TEXT_OR_NULL_TARGET.method, "GetTextOrNull");
        assert_eq!(LOCALIZATION_GET_TEXT_OR_NULL_TARGET.return_type, "string");
        assert_eq!(
            LOCALIZATION_GET_TEXT_OR_NULL_TARGET.parameter_types,
            &["string", "int"]
        );
        assert_eq!(LOCALIZATION_GET_TEXT_TARGET.method, "GetText");
        assert_eq!(
            LOCALIZATION_GET_TEXT_TARGET.parameter_types,
            &["string", "int"]
        );
    }

    #[test]
    fn record_round_trips_utf16_and_rejects_oversized_values() {
        let category: Vec<_> = "剧情".encode_utf16().collect();
        let text: Vec<_> = "きらめく世界".encode_utf16().collect();
        let record = DumpRecord::capture(&category, 42, &text).expect("bounded record");
        assert_eq!(
            record.decode(),
            Ok((
                "剧情".to_owned(),
                "42".to_owned(),
                "きらめく世界".to_owned()
            ))
        );
        assert!(DumpRecord::capture(&vec![0; CATEGORY_UTF16_CAPACITY + 1], 0, &[]).is_none());
        assert!(DumpRecord::capture(&[], 0, &vec![0; TEXT_UTF16_CAPACITY + 1]).is_none());
    }

    #[test]
    fn merge_flush_and_resume_preserve_localify_shape() {
        let root = temp_path("merge");
        let path = root.join("dumps").join("localify.json");
        let _ = std::fs::remove_dir_all(&root);
        let diagnostics = Arc::new(DumpDiagnostics::default());
        let mut state = DumpState::load(path.clone(), Arc::clone(&diagnostics)).expect("load");
        let category: Vec<_> = "Menu".encode_utf16().collect();
        let text: Vec<_> = "Start".encode_utf16().collect();
        state.merge(&DumpRecord::capture(&category, 7, &text).expect("record"));
        assert!(state.flush().expect("flush"));

        let resumed = DumpState::load(path, diagnostics).expect("resume");
        assert_eq!(resumed.entries["Menu"]["7"], "Start");
        assert!(!resumed.dirty);
        let _ = std::fs::remove_dir_all(root);
    }
}
