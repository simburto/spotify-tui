// #![windows_subsystem = "windows"]

mod qemu;
mod spotify;

use qemu::{QemuBackend, QemuConfig};
use anyhow::{Context, Result};
use eframe::egui::{self, Color32, FontId, Pos2, Rect, RichText, Rounding, Stroke, Vec2};
use futures_util::{SinkExt, StreamExt};
use rtrb::{Consumer, RingBuffer};
use rubato::{FastFixedIn, Resampler};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
use std::ffi::c_void;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use std::sync::OnceLock;

static FIGLET_FONT: OnceLock<Option<figlet_rs::FIGfont>> = OnceLock::new();

use spotify::{
    ArtistPageData, SearchResultItem, SearchResultKind, SpotifyDeviceItem, SpotifyFollowedArtistItem,
    SpotifyManager, SpotifyPlaylistItem, SpotifyResponse, SpotifySavedAlbumItem,
    SpotifyTrackItem,
};

use windows::{
    core::*,
    Win32::Foundation::*,
    Win32::Media::Audio::*,
    Win32::System::Com::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
    Win32::System::Threading::*,
    Win32::UI::WindowsAndMessaging::*,
};

const GUEST_SAMPLE_RATE: usize = 44_100;
const GUEST_CHANNELS: usize = 2;
const BYTES_PER_SAMPLE: usize = 3;
const BYTES_PER_FRAME: usize = BYTES_PER_SAMPLE * GUEST_CHANNELS;
const PREBUFFER_BYTES: usize = (GUEST_SAMPLE_RATE / 4) * BYTES_PER_FRAME;
const RING_BUFFER_CAPACITY_BYTES: usize = GUEST_SAMPLE_RATE * BYTES_PER_FRAME * 2;

// --- Rosé Pine Palette (Aligned with 0x2106 / 0xDCF2 hardware project) ---
pub const COLOR_BG: Color32 = Color32::from_rgb(25, 23, 36);          // _base (#191724 / 0x2106)
pub const COLOR_SURFACE: Color32 = Color32::from_rgb(31, 29, 46);     // _surface (#1f1d2e)
pub const COLOR_OVERLAY: Color32 = Color32::from_rgb(38, 35, 58);     // _overlay (#26233a)
pub const COLOR_MUTED: Color32 = Color32::from_rgb(110, 106, 134);    // _muted (#6e6a86 / 0xD69A)
pub const COLOR_SUBTLE: Color32 = Color32::from_rgb(144, 140, 170);   // _subtle (#908caa)
pub const COLOR_TEXT_BRIGHT: Color32 = Color32::from_rgb(224, 222, 244); // _text (#e0def4)
pub const COLOR_TEXT_DIM: Color32 = COLOR_SUBTLE;

// Accent & Control Roles
pub const COLOR_ACCENT_GREEN: Color32 = Color32::from_rgb(156, 207, 216); // Foam (#9ccfd8 / 0xDCF2)
pub const COLOR_ACCENT_ROSE: Color32 = Color32::from_rgb(235, 188, 186);  // Rose (#ebbcba)
pub const COLOR_ACCENT_GOLD: Color32 = Color32::from_rgb(246, 193, 119);  // Gold (#f6c177 / 0xFFE0)
pub const COLOR_ACCENT_LOVE: Color32 = Color32::from_rgb(235, 111, 146);  // Love (#eb6f92)
pub const COLOR_BORDER: Color32 = Color32::from_rgb(68, 65, 90);          // _highlight_med (#44415a)

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct AppConfig {
    client_id: String,
}

impl AppConfig {
    fn path() -> PathBuf {
        PathBuf::from("config.json")
    }

    fn load() -> Self {
        if let Ok(data) = fs::read_to_string(Self::path()) {
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(self)?;
        fs::write(Self::path(), data)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
enum SoloistCommand {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Seek(u64),
    PlayUri(String),
    PlayContext {
        uri: String,
        context_uri: Option<String>,
        offset_index: Option<usize>,
    },
    ToggleShuffle,
    CycleRepeat,
    Activate,
    GetState,
    GetQueue,
    SetVolume(u8),
    SetShuffle(bool),
    SetRepeatContext(bool),
    SetRepeatTrack(bool),
}

#[derive(Debug, Clone)]
enum SpotifyRequest {
    Init(String),
    Reauthenticate,
    RefreshLibrary,
    Search(String),
    FetchLikedSongs,
    FetchPlaylist(String),
    FetchAlbum(String),
    FetchArtist(String),
    StartPlayback {
        uri: String,
        context_uri: Option<String>,
        upcoming_uris: Vec<String>,
    },
    FetchArtistBio(String),
    CheckLibraryContext(String),
    CheckLibraryTrack(String),
    ToggleLibraryUri {
        uri: String,
        current_state: bool,
        is_context: bool,
    },
    // Context menu actions:
    AddToPlaylist {
        playlist_id: String,
        track_uri: String,
    },
    RemoveFromPlaylist {
        playlist_id: String,
        updated_uris: Vec<String>,
    },
    AddToQueue {
        track_uri: String,
    },
    FetchDevices,
    TransferPlayback {
        device_id: String,
        play: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum NavLocation {
    Home,
    LikedSongs,
    Playlist { id: String, title: String },
    Album { id: String, title: String },
    Artist { id: String },
    Search { query: String },
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryFilter {
    All,
    Playlists,
    Artists,
    Albums,
}

#[derive(Debug, Clone)]
pub struct TrackContextMenu {
    pub track: SpotifyTrackItem,
    pub click_pos: Pos2,
    pub filter_query: String,
    pub show_playlist_submenu: bool,
    pub is_saved: bool,
    pub track_index: usize,
}

#[derive(Debug, Clone)]
struct AppState {
    title: String,
    artist: String,
    album: String,
    cover_url: Option<String>,
    is_playing: bool,
    connected_ws: bool,
    connected_audio: bool,
    position_ms: u64,
    duration_ms: u64,
    last_sync: Instant,

    // Spotify data
    search_query: String,
    displayed_tracks: Vec<SpotifyTrackItem>,
    search_results: Vec<SearchResultItem>,
    artist_page: Option<ArtistPageData>,
    user_playlists: Vec<SpotifyPlaylistItem>,
    selected_track_uri: Option<String>,
    active_nav: String,
    view_title: String,

    // Modal state for first launch key prompt
    show_key_prompt: bool,
    input_client_id: String,
    key_prompt_error: Option<String>,
    active_context_uri: Option<String>,
    history: Vec<NavLocation>,
    future: Vec<NavLocation>,
    current_location: Option<NavLocation>,
    pub shuffle_state: bool,
    pub repeat_state: u8,
    pub playback_speed: f64,
    pub is_dragging_scrubber: bool,
    pub scrubber_drag_val: f64,
    pub volume: u8,
    queue_items: Vec<SpotifyTrackItem>,
    pub show_queue: bool,
    pub user_name: Option<String>,
    pub user_avatar_url: Option<String>,
    pub left_col_width: f32,
    pub right_col_width: f32,
    pub artist_bio: Option<String>,
    pub artist_followers: Option<u64>,
    pub artist_monthly_listeners: Option<u64>,
    pub current_artist_id: Option<String>,
    pub artist_info_cache: std::collections::HashMap<String, (String, u64, u64)>,
    pub pending_artist_fetches: std::collections::HashSet<String>,
    pub artist_bio_expanded: bool,
    pub playlist_total_tracks: Option<usize>,
    pub is_loading_view: bool,
    pub user_followed_artists: Vec<SpotifyFollowedArtistItem>,
    pub user_saved_albums: Vec<SpotifySavedAlbumItem>,
    pub library_filter: LibraryFilter,
    pub is_current_item_saved: bool,
    pub current_user_id: Option<String>,
    pub is_current_item_owner: bool,
    pub is_context_saved: Option<bool>, // None = loading check, Some(true) = saved, Some(false) = not saved
    pub is_context_owner: bool,
    pub is_track_saved: bool,
    pub current_album_id: Option<String>,
    pub current_artists: Vec<(String, String)>, // (Name, Artist ID)
    pub context_menu: Option<TrackContextMenu>,
    pub liked_track_uris: std::collections::HashSet<String>,
    pub last_library_sync: Instant,
    pub available_devices: Vec<SpotifyDeviceItem>,
    pub show_device_menu: bool,
    pub manual_queue_items: Vec<SpotifyTrackItem>,
    pub next_up_items: Vec<SpotifyTrackItem>,
    pub qemu_running: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            title: "Waiting for playback...".into(),
            artist: "-".into(),
            album: "-".into(),
            cover_url: None,
            is_playing: false,
            connected_ws: false,
            connected_audio: false,
            position_ms: 0,
            duration_ms: 0,
            last_sync: Instant::now(),
            search_query: String::new(),
            displayed_tracks: Vec::new(),
            search_results: Vec::new(),
            artist_page: None,
            user_playlists: Vec::new(),
            selected_track_uri: None,
            active_nav: "Home".into(),
            active_context_uri: None,
            view_title: "Spotatui Home".into(),
            history: Vec::new(),
            future: Vec::new(),
            current_location: Some(NavLocation::Home),
            show_key_prompt: false,
            input_client_id: String::new(),
            key_prompt_error: None,
            shuffle_state: false,
            repeat_state: 0,
            playback_speed: 1.0,
            is_dragging_scrubber: false,
            scrubber_drag_val: 0.0,
            volume: 100,
            queue_items: Vec::new(),
            show_queue: false,
            user_name: None,
            user_avatar_url: None,
            left_col_width: 200.0,
            right_col_width: 220.0,
            artist_bio: None,
            artist_followers: None,
            artist_monthly_listeners: None,
            current_artist_id: None,
            artist_info_cache: std::collections::HashMap::new(),
            pending_artist_fetches: std::collections::HashSet::new(),
            artist_bio_expanded: false,
            playlist_total_tracks: None,
            is_loading_view: false,
            user_followed_artists: Vec::new(),
            user_saved_albums: Vec::new(),
            library_filter: LibraryFilter::All,
            is_current_item_saved: false,
            current_user_id: None,
            is_current_item_owner: false,
            is_context_saved: None,
            is_context_owner: false,
            is_track_saved: false,
            current_album_id: None,
            current_artists: Vec::new(),
            context_menu: None,
            liked_track_uris: std::collections::HashSet::new(),
            last_library_sync: Instant::now(),
            available_devices: Vec::new(),
            show_device_menu: false,
            manual_queue_items: Vec::new(),
            next_up_items: Vec::new(),
            qemu_running: false,
        }
    }
}

struct SoloistApp {
    state: Arc<std::sync::RwLock<AppState>>,
    cmd_tx: mpsc::Sender<SoloistCommand>,
    spotify_req_tx: mpsc::Sender<SpotifyRequest>,
    spotify_resp_rx: std::sync::Mutex<mpsc::Receiver<SpotifyResponse>>,
    audio_connected: Arc<AtomicBool>,
    ws_connected: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
enum BioSegment {
    Text(String),
    Link { text: String, uri: String },
}

fn generate_ascii_banner(raw_text: &str) -> Option<(String, usize)> {
    // Only skip if text contains non-ASCII characters (e.g. CJK, emojis)
    if !raw_text.is_ascii() {
        return None;
    }

    let font_option = FIGLET_FONT.get_or_init(|| {
        const SLANT_FONT: &str = include_str!("../assets/larry3d.flf");
        figlet_rs::FIGfont::from_content(SLANT_FONT).ok().or_else(|| {
            figlet_rs::FIGfont::standard().ok()
        })
    });

    if let Some(font) = font_option.as_ref() {
        if let Some(figure) = font.convert(raw_text) {
            let rendered = figure.to_string();
            let max_line_len = rendered.lines().map(|l| l.chars().count()).max().unwrap_or(0);
            if max_line_len > 0 {
                return Some((rendered, max_line_len));
            }
        }
    }

    None
}

fn wrap_title_words(raw_text: &str, max_line_chars: usize) -> Vec<String> {
    let words: Vec<&str> = raw_text.split_whitespace().collect();
    if words.is_empty() {
        return vec![raw_text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();

    for word in words {
        if current_line.is_empty() {
            current_line.push_str(word);
        } else if current_line.chars().count() + 1 + word.chars().count() <= max_line_chars {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            lines.push(current_line);
            current_line = word.to_string();
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
}

fn generate_wrapped_ascii_banner(raw_text: &str, max_line_chars: usize) -> Option<(String, usize)> {
    if !raw_text.is_ascii() {
        return None;
    }

    let font_option = FIGLET_FONT.get_or_init(|| {
        const SLANT_FONT: &str = include_str!("../assets/larry3d.flf");
        figlet_rs::FIGfont::from_content(SLANT_FONT).ok().or_else(|| {
            figlet_rs::FIGfont::standard().ok()
        })
    });

    let font = font_option.as_ref()?;
    let wrapped_lines = wrap_title_words(raw_text, max_line_chars);

    let mut full_banner = String::new();
    let mut overall_max_cols = 0;

    for (idx, line) in wrapped_lines.iter().enumerate() {
        if let Some(figure) = font.convert(line) {
            let rendered = figure.to_string();
            let line_max_cols = rendered.lines().map(|l| l.chars().count()).max().unwrap_or(0);
            overall_max_cols = overall_max_cols.max(line_max_cols);

            if idx > 0 {
                full_banner.push('\n');
            }
            full_banner.push_str(&rendered);
        }
    }

    if overall_max_cols > 0 {
        Some((full_banner, overall_max_cols))
    } else {
        None
    }
}

fn render_dynamic_ascii_banner(ui: &mut egui::Ui, text: &str, color: Color32) {
    let sanitized_text = text.trim();
    if sanitized_text.is_empty() {
        return;
    }

    // Determine target wrap threshold based on container width.
    // At standard widths (~600-800px), 14-16 characters per line keeps larry3d at a readable ~7-9pt.
    let avail_w = (ui.available_width() - 16.0).max(80.0);
    let target_wrap_chars = if avail_w < 450.0 {
        10
    } else if avail_w < 650.0 {
        14
    } else {
        18
    };

    if let Some((banner, max_cols)) = generate_wrapped_ascii_banner(sanitized_text, target_wrap_chars) {
        let char_aspect_ratio = 0.65_f32;
        let target_char_w = avail_w / max_cols as f32;

        // Scale dynamically down to fit width without clipping, keeping between 5.0pt and 8.5pt
        let dynamic_font_size = (target_char_w / char_aspect_ratio).clamp(5.0, 8.5);
        let dynamic_line_height = dynamic_font_size * 1.05;

        // Use a standard label so egui properly calculates the layout height,
        // preventing subsequent UI (stats line, table header) from being drawn on top of the banner.
        ui.add(
            egui::Label::new(
                RichText::new(banner)
                    .font(FontId::monospace(dynamic_font_size))
                    .line_height(Some(dynamic_line_height))
                    .color(color),
            )
                .wrap_mode(egui::TextWrapMode::Extend),
        );
    } else {
        // Fallback for non-ASCII text (e.g. CJK, emojis)
        ui.label(
            RichText::new(text)
                .font(FontId::monospace(20.0))
                .strong()
                .color(color),
        );
    }
}

fn parse_spotify_html_bio(raw: &str) -> Vec<BioSegment> {
    let mut segments = Vec::new();
    let mut cursor = 0;

    while cursor < raw.len() {
        if let Some(tag_start) = raw[cursor..].find("<a ") {
            let abs_start = cursor + tag_start;
            if abs_start > cursor {
                segments.push(BioSegment::Text(raw[cursor..abs_start].to_string()));
            }

            if let Some(href_pos) = raw[abs_start..].find("href=\"") {
                let url_start = abs_start + href_pos + 6;
                if let Some(url_end_offset) = raw[url_start..].find('"') {
                    let uri = raw[url_start..url_start + url_end_offset].to_string();

                    if let Some(tag_close) = raw[url_start + url_end_offset..].find('>') {
                        let inner_start = url_start + url_end_offset + tag_close + 1;
                        if let Some(end_tag) = raw[inner_start..].find("</a>") {
                            let text = raw[inner_start..inner_start + end_tag].to_string();
                            segments.push(BioSegment::Link { text, uri });
                            cursor = inner_start + end_tag + 4;
                            continue;
                        }
                    }
                }
            }
            // Fallback if tag was malformed
            segments.push(BioSegment::Text("<a ".to_string()));
            cursor = abs_start + 3;
        } else {
            segments.push(BioSegment::Text(raw[cursor..].to_string()));
            break;
        }
    }

    segments
}

impl AppState {
    fn navigate_to(&mut self, target: NavLocation) {
        if self.current_location.as_ref() == Some(&target) {
            return;
        }
        if let Some(curr) = self.current_location.take() {
            self.history.push(curr);
        }
        self.future.clear();
        self.current_location = Some(target);
    }

    fn can_go_back(&self) -> bool {
        !self.history.is_empty()
    }

    fn can_go_forward(&self) -> bool {
        !self.future.is_empty()
    }

    fn go_back(&mut self) -> Option<NavLocation> {
        if let Some(prev) = self.history.pop() {
            if let Some(curr) = self.current_location.take() {
                self.future.push(curr);
            }
            self.current_location = Some(prev.clone());
            Some(prev)
        } else {
            None
        }
    }

    fn go_forward(&mut self) -> Option<NavLocation> {
        if let Some(next) = self.future.pop() {
            if let Some(curr) = self.current_location.take() {
                self.history.push(curr);
            }
            self.current_location = Some(next.clone());
            Some(next)
        } else {
            None
        }
    }
}

fn dispatch_nav(target: NavLocation, state: &mut AppState, req_tx: &mpsc::Sender<SpotifyRequest>) {
    match target {
        NavLocation::Home => {
            state.active_nav = "Home".into();
            state.view_title = "Spotatui Home".into();
            state.displayed_tracks.clear();
            state.search_results.clear();
            state.artist_page = None;
            state.active_context_uri = None;
            state.is_loading_view = false;
        }
        NavLocation::Queue => {
            state.active_nav = "Queue".into();
            state.view_title = "Queue".into();
            state.is_loading_view = false;
        }
        NavLocation::LikedSongs => {
            state.active_nav = "Liked Songs".into();
            state.view_title = "Liked Songs".into();
            state.artist_page = None;
            state.search_results.clear();
            state.displayed_tracks.clear();
            state.active_context_uri = None;
            let _ = req_tx.blocking_send(SpotifyRequest::FetchLikedSongs);
        }
        NavLocation::Playlist { id, title } => {
            let clean_id = id.trim_start_matches("spotify:playlist:").to_string();
            let uri = format!("spotify:playlist:{}", clean_id);
            state.active_nav = title.clone();
            state.view_title = title;
            state.artist_page = None;
            state.search_results.clear();
            state.displayed_tracks.clear();
            state.active_context_uri = Some(uri.clone());
            state.playlist_total_tracks = None;

            // 1. Check ownership
            let is_user_owned = state.user_playlists.iter().any(|pl| {
                pl.id == clean_id && state.current_user_id.as_deref() == Some(&pl.owner_id)
            });
            state.is_context_owner = is_user_owned;

            // 2. Immediate local cache check
            let is_in_library = state.user_playlists.iter().any(|pl| pl.id == clean_id);
            state.is_context_saved = if is_user_owned {
                Some(true)
            } else if is_in_library {
                Some(true)
            } else {
                None // Will be populated by CheckLibraryContext
            };

            let _ = req_tx.blocking_send(SpotifyRequest::FetchPlaylist(clean_id));
            if !is_user_owned {
                let _ = req_tx.blocking_send(SpotifyRequest::CheckLibraryContext(uri));
            }
        }
        NavLocation::Album { id, title } => {
            let clean_id = id.trim_start_matches("spotify:album:").to_string();
            let uri = format!("spotify:album:{}", clean_id);
            state.view_title = format!("Album: {}", title);
            state.is_loading_view = true;
            state.active_context_uri = Some(uri.clone());
            state.is_context_owner = false;

            // Immediate local cache check
            let is_in_saved = state.user_saved_albums.iter().any(|alb| alb.id == clean_id);
            state.is_context_saved = Some(is_in_saved);

            let _ = req_tx.blocking_send(SpotifyRequest::CheckLibraryContext(uri));
            let _ = req_tx.blocking_send(SpotifyRequest::FetchAlbum(clean_id));
        }
        NavLocation::Artist { id } => {
            let clean_id = id.trim_start_matches("spotify:artist:").to_string();
            let uri = format!("spotify:artist:{}", clean_id);
            state.active_nav = uri.clone();
            state.view_title = "Artist".into();
            state.artist_bio_expanded = false;
            state.artist_page = None;
            state.displayed_tracks.clear();
            state.search_results.clear();
            state.active_context_uri = Some(uri.clone());
            state.is_loading_view = true;
            state.is_context_owner = false;

            // Immediate local cache check
            let is_following = state.user_followed_artists.iter().any(|art| art.id == clean_id);
            state.is_context_saved = Some(is_following);

            let _ = req_tx.blocking_send(SpotifyRequest::CheckLibraryContext(uri));
            let _ = req_tx.blocking_send(SpotifyRequest::FetchArtist(clean_id));
        }
        NavLocation::Search { query } => {
            state.search_query = query.clone();
            state.is_loading_view = true;
            let _ = req_tx.blocking_send(SpotifyRequest::Search(query));
        }
    }
}

fn render_v_splitter(
    ui: &mut egui::Ui,
    _id_str: &str,
    height: f32,
    width_ref: &mut f32,
    min_w: f32,
    max_w: f32,
    invert: bool,
) {
    let splitter_w = 4.0;
    let (rect, resp) = ui.allocate_at_least(Vec2::new(splitter_w, height), egui::Sense::drag());

    let is_hovered = resp.hovered();
    let is_dragged = resp.dragged();

    if is_hovered || is_dragged {
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::ResizeHorizontal);
    }

    if is_dragged {
        let delta = ui.input(|i| i.pointer.delta().x);
        if invert {
            *width_ref = (*width_ref - delta).clamp(min_w, max_w);
        } else {
            *width_ref = (*width_ref + delta).clamp(min_w, max_w);
        }
    }

    // Draw a line indicator when active or hovered
    let col = if is_dragged {
        COLOR_ACCENT_ROSE
    } else if is_hovered {
        Color32::from_rgb(140, 145, 175)
    } else {
        Color32::TRANSPARENT
    };

    ui.painter().rect_filled(
        Rect::from_center_size(rect.center(), Vec2::new(1.0, height - 10.0)),
        Rounding::ZERO,
        col,
    );
}

/// Renders a crisp vector device monitor icon instead of low-res bitmap emojis
fn draw_crisp_device_icon(painter: &egui::Painter, center: Pos2, color: Color32) {
    let w = 12.0_f32;
    let h = 9.0_f32;
    let screen_rect = Rect::from_center_size(Pos2::new(center.x, center.y - 1.5), Vec2::new(w, h));
    painter.rect_stroke(screen_rect, Rounding::ZERO, Stroke::new(1.0_f32, color));

    // Stand & base
    painter.line_segment(
        [Pos2::new(center.x, screen_rect.max.y), Pos2::new(center.x, screen_rect.max.y + 2.5)],
        Stroke::new(1.0_f32, color),
    );
    painter.line_segment(
        [Pos2::new(center.x - 3.5, screen_rect.max.y + 2.5), Pos2::new(center.x + 3.5, screen_rect.max.y + 2.5)],
        Stroke::new(1.0_f32, color),
    );
}

/// Renders a sharp vector speaker icon
fn draw_crisp_speaker_icon(painter: &egui::Painter, center: Pos2, color: Color32, is_muted: bool) {
    let body_rect = Rect::from_center_size(Pos2::new(center.x - 3.0, center.y), Vec2::new(3.0, 4.0));
    painter.rect_filled(body_rect, Rounding::ZERO, color);

    let cone = [
        Pos2::new(center.x - 2.0, center.y - 2.0),
        Pos2::new(center.x + 1.5, center.y - 5.0),
        Pos2::new(center.x + 1.5, center.y + 5.0),
        Pos2::new(center.x - 2.0, center.y + 2.0),
    ];
    painter.add(egui::epaint::PathShape::convex_polygon(cone.to_vec(), color, Stroke::NONE));

    if is_muted {
        painter.line_segment(
            [Pos2::new(center.x + 3.5, center.y - 3.5), Pos2::new(center.x + 7.5, center.y + 3.5)],
            Stroke::new(1.0_f32, color),
        );
        painter.line_segment(
            [Pos2::new(center.x + 7.5, center.y - 3.5), Pos2::new(center.x + 3.5, center.y + 3.5)],
            Stroke::new(1.0_f32, color),
        );
    } else {
        // Sound wave arc
        painter.line_segment(
            [Pos2::new(center.x + 4.0, center.y - 3.0), Pos2::new(center.x + 5.5, center.y)],
            Stroke::new(1.0_f32, color),
        );
        painter.line_segment(
            [Pos2::new(center.x + 5.5, center.y), Pos2::new(center.x + 4.0, center.y + 3.0)],
            Stroke::new(1.0_f32, color),
        );
    }
}

impl SoloistApp {
    fn new(
        state: Arc<std::sync::RwLock<AppState>>,
        cmd_tx: mpsc::Sender<SoloistCommand>,
        spotify_req_tx: mpsc::Sender<SpotifyRequest>,
        spotify_resp_rx: mpsc::Receiver<SpotifyResponse>,
        audio_connected: Arc<AtomicBool>,
        ws_connected: Arc<AtomicBool>,
    ) -> Self {
        Self {
            state,
            cmd_tx,
            spotify_req_tx,
            spotify_resp_rx: std::sync::Mutex::new(spotify_resp_rx),
            audio_connected,
            ws_connected,
        }
    }

    fn render_tui_box<R>(
        &self,
        ui: &mut egui::Ui,
        title: &str,
        is_active: bool,
        size: Vec2,
        add_contents: impl FnOnce(&mut egui::Ui) -> R,
    ) -> egui::Response {
        let safe_size = Vec2::new(size.x.max(0.0), size.y.max(0.0));
        let (rect, resp) = ui.allocate_exact_size(safe_size, egui::Sense::hover());
        let bg_color = ui.visuals().panel_fill;

        // Direct pointer intersection check ensures children don't mask box hover
        let is_hovered = ui.input(|i| {
            i.pointer.hover_pos().map_or(false, |p| rect.contains(p))
        });

        let border_color = if is_hovered {
            if is_active {
                COLOR_ACCENT_ROSE
            } else {
                COLOR_ACCENT_ROSE
            }
        } else if is_active {
            COLOR_ACCENT_ROSE
        } else {
            COLOR_BORDER // Muted highlight_med wireframe
        };

        let title_color = if is_hovered {
            COLOR_TEXT_BRIGHT
        } else if is_active {
            COLOR_ACCENT_ROSE
        } else {
            COLOR_MUTED
        };

        // Fill matching COLOR_BG
        let painter = ui.painter();
        painter.rect_filled(rect, Rounding::ZERO, COLOR_BG);
        painter.rect_stroke(rect, Rounding::ZERO, Stroke::new(1.0_f32, border_color));

        // 4. Top-left cutout title badge
        if !title.is_empty() {
            let font_id = FontId::monospace(10.5);
            let title_galley = painter.layout_no_wrap(title.to_string(), font_id, title_color);
            let text_size = title_galley.size();

            // Punch out a small segment using the matching background color
            let pad_x = 4.0;
            let cutout_rect = Rect::from_min_size(
                Pos2::new(rect.min.x + 10.0 - pad_x, rect.min.y - (text_size.y / 2.0)),
                Vec2::new(text_size.x + (pad_x * 2.0), text_size.y),
            );
            painter.rect_filled(cutout_rect, Rounding::ZERO, bg_color);

            // Draw title text centered inside cutout
            painter.galley(
                Pos2::new(rect.min.x + 10.0, rect.min.y - (text_size.y / 2.0)),
                title_galley,
                title_color,
            );
        }

        // Render Box Children
        let child_rect = Rect::from_min_max(
            Pos2::new(rect.min.x + 8.0, rect.min.y + 14.0),
            Pos2::new(rect.max.x - 8.0, rect.max.y - 8.0),
        );

        let mut child_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(title)
                .max_rect(child_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        // Clip up to 2px inside the actual box stroke border instead of 8px inside
        let clip_rect = Rect::from_min_max(
            Pos2::new(rect.min.x + 2.0, rect.min.y + 2.0),
            Pos2::new(rect.max.x - 2.0, rect.max.y - 2.0),
        );
        child_ui.set_clip_rect(clip_rect);
        add_contents(&mut child_ui);

        resp
    }
    fn render_artist_meta_fallback(&self, ui: &mut egui::Ui, st: &AppState) {
        ui.vertical(|ui| {
            // Check if full artist page is cached, or fallback to state fields
            let followers = st.artist_followers
                .or_else(|| st.artist_page.as_ref().map(|p| p.followers));
            let monthly = st.artist_monthly_listeners
                .or_else(|| st.artist_page.as_ref().map(|p| p.monthly_listeners));

            if let Some(f) = followers {
                let f_str = if f >= 1_000_000 {
                    format!("{:.1}M", f as f64 / 1_000_000.0)
                } else if f >= 1_000 {
                    format!("{:.0}K", f as f64 / 1_000.0)
                } else {
                    format!("{}", f)
                };

                ui.label(
                    RichText::new(format!("👥 {} followers", f_str))
                        .font(FontId::monospace(10.5))
                        .color(COLOR_SUBTLE),
                );
            }

            if let Some(m) = monthly {
                let m_str = if m >= 1_000_000 {
                    format!("{:.1}M", m as f64 / 1_000_000.0)
                } else if m >= 1_000 {
                    format!("{:.0}K", m as f64 / 1_000.0)
                } else {
                    format!("{}", m)
                };

                ui.add_space(2.0);
                ui.label(
                    RichText::new(format!("🎧 {} monthly listeners", m_str))
                        .font(FontId::monospace(10.5))
                        .color(COLOR_SUBTLE),
                );
            }

            if followers.is_none() && monthly.is_none() {
                ui.label(
                    RichText::new("Verified Spotify Artist")
                        .font(FontId::monospace(10.0))
                        .color(COLOR_MUTED),
                );
            }
        });
    }
    fn render_bio_segments(
        &self,
        ui: &mut egui::Ui,
        st: &mut AppState,
        segments: &[BioSegment],
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(0.0, 3.0);

            for seg in segments {
                match seg {
                    BioSegment::Text(text) => {
                        // Decode common HTML entities from Spotify API
                        let decoded = text
                            .replace("&amp;", "&")
                            .replace("&quot;", "\"")
                            .replace("&#39;", "'")
                            .replace("&lt;", "<")
                            .replace("&gt;", ">");

                        ui.label(
                            RichText::new(decoded)
                                .font(FontId::monospace(11.0))
                                .italics()
                                .color(COLOR_TEXT_BRIGHT),
                        );
                    }
                    BioSegment::Link { text, uri } => {
                        let btn = egui::Button::new(
                            RichText::new(text)
                                .font(FontId::monospace(11.0))
                                .underline()
                                .color(COLOR_ACCENT_ROSE),
                        )
                            .fill(Color32::TRANSPARENT)
                            .frame(false);

                        if ui.add(btn).clicked() {
                            if uri.starts_with("spotify:artist:") {
                                let id = uri.trim_start_matches("spotify:artist:").to_string();
                                st.navigate_to(NavLocation::Artist { id: id.clone() });
                                dispatch_nav(NavLocation::Artist { id }, st, &self.spotify_req_tx);
                            } else if uri.starts_with("spotify:search:") {
                                let query = uri.trim_start_matches("spotify:search:").to_string();
                                // Clean up URL-encoded label queries like label%3A%22Roc%22
                                let clean_q = query.replace("%22", "\"").replace("%3A", ":").replace('+', " ");
                                st.navigate_to(NavLocation::Search { query: clean_q.clone() });
                                dispatch_nav(NavLocation::Search { query: clean_q }, st, &self.spotify_req_tx);
                            } else if uri.starts_with("spotify:album:") {
                                let id = uri.trim_start_matches("spotify:album:").to_string();
                                st.navigate_to(NavLocation::Album { id: id.clone(), title: text.clone() });
                                dispatch_nav(NavLocation::Album { id, title: text.clone() }, st, &self.spotify_req_tx);
                            }
                        }
                    }
                }
            }
        });
    }
    fn render_inline_link(
        ui: &mut egui::Ui,
        text: &str,
        default_color: Color32,
        font_id: FontId,
        max_width: f32,
    ) -> egui::Response {
        let text_clean = text.trim();
        let target_w = max_width.max(10.0);

        let (rect, resp) = ui.allocate_exact_size(Vec2::new(target_w, 16.0), egui::Sense::click());

        let is_hovered = resp.hovered();
        let color = if is_hovered {
            COLOR_ACCENT_ROSE
        } else {
            default_color
        };

        let mut rich = RichText::new(text_clean).font(font_id).color(color);
        if is_hovered {
            rich = rich.underline();
        }

        let widget_text: egui::WidgetText = rich.into();
        let galley = widget_text.into_galley(
            ui,
            Some(egui::TextWrapMode::Truncate),
            target_w,
            egui::TextStyle::Body,
        );

        // Intersect with ui.clip_rect() so it NEVER draws outside the scroll container
        let mut painter = ui.painter().clone();
        let effective_clip = ui.clip_rect().intersect(rect);
        painter.set_clip_rect(effective_clip);
        painter.galley(rect.min, galley, color);

        if is_hovered {
            ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
        }

        resp
    }
    fn render_artist_links(
        ui: &mut egui::Ui,
        artists: &[(String, String)],
        default_color: Color32,
        font_id: FontId,
        nav_action: &mut Option<NavLocation>,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(0.0, 0.0);

            for (i, (name, id)) in artists.iter().enumerate() {
                if i > 0 {
                    ui.label(RichText::new(", ").font(font_id.clone()).color(COLOR_MUTED));
                }

                let link_galley = ui.painter().layout_no_wrap(name.clone(), font_id.clone(), default_color);
                let link_w = link_galley.size().x;
                let resp = Self::render_inline_link(ui, name, default_color, font_id.clone(), link_w);

                if resp.clicked() && !id.is_empty() {
                    *nav_action = Some(NavLocation::Artist { id: id.clone() });
                }
            }
        });
    }
    fn render_inline_link_tight(
        ui: &mut egui::Ui,
        text: &str,
        default_color: Color32,
        font_id: FontId,
        max_w: f32,
    ) -> egui::Response {
        let text_clean = text.trim();
        let galley = ui.painter().layout(
            text_clean.to_string(),
            font_id.clone(),
            default_color,
            max_w.max(10.0),
        );
        let natural_w = galley.size().x.min(max_w).max(10.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(natural_w, 16.0), egui::Sense::click());

        let is_hovered = resp.hovered();
        let color = if is_hovered { COLOR_ACCENT_ROSE } else { default_color };

        let mut rich = RichText::new(text_clean).font(font_id).color(color);
        if is_hovered {
            rich = rich.underline();
        }

        let widget_text: egui::WidgetText = rich.into();
        let draw_galley = widget_text.into_galley(
            ui,
            Some(egui::TextWrapMode::Truncate),
            natural_w,
            egui::TextStyle::Body,
        );

        let mut painter = ui.painter().clone();
        painter.set_clip_rect(ui.clip_rect().intersect(rect));
        painter.galley(rect.min, draw_galley, color);

        if is_hovered {
            ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
        }
        resp
    }
}

impl eframe::App for SoloistApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        let screen_rect = ctx.screen_rect();
        if is_minimized || screen_rect.height() <= 0.0 || screen_rect.width() <= 0.0 {
            return;
        }

        // Periodically refresh the sidebar library every 60 seconds
        let should_refresh_library = {
            let st = self.state.read().unwrap();
            st.last_library_sync.elapsed() >= Duration::from_secs(60)
        };

        if should_refresh_library {
            if let Ok(mut st) = self.state.write() {
                st.last_library_sync = Instant::now();
            }
            let _ = self.spotify_req_tx.try_send(SpotifyRequest::RefreshLibrary);
        }

        let is_maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));

        if let Ok(mut rx) = self.spotify_resp_rx.try_lock() {
            while let Ok(msg) = rx.try_recv() {
                let mut st = self.state.write().unwrap();
                match msg {
                    SpotifyResponse::PlaylistInitial { playlist_id, tracks, total } => {
                        let expected_uri = format!("spotify:playlist:{}", playlist_id);
                        if st.active_context_uri.as_ref() == Some(&expected_uri) {
                            st.displayed_tracks = tracks;
                            st.playlist_total_tracks = Some(total);
                            st.is_loading_view = false;
                        }
                    }
                    SpotifyResponse::PlaylistChunk { playlist_id, tracks } => {
                        let expected_uri = format!("spotify:playlist:{}", playlist_id);
                        if st.active_context_uri.as_ref() == Some(&expected_uri) {
                            st.displayed_tracks.extend(tracks);
                        }
                    }
                }
            }
        }

        // --- Window Border & Corner Drag-to-Resize (Only active when not maximized) ---
        if !is_maximized {
            let border_size = 6.0_f32;
            let rect = screen_rect;

            let edges = [
                // Left, Right, Top, Bottom
                (Rect::from_min_size(rect.min, Vec2::new(border_size, rect.height())), egui::ResizeDirection::West),
                (Rect::from_min_size(Pos2::new(rect.max.x - border_size, rect.min.y), Vec2::new(border_size, rect.height())), egui::ResizeDirection::East),
                (Rect::from_min_size(rect.min, Vec2::new(rect.width(), border_size)), egui::ResizeDirection::North),
                (Rect::from_min_size(Pos2::new(rect.min.x, rect.max.y - border_size), Vec2::new(rect.width(), border_size)), egui::ResizeDirection::South),
                // Corners
                (Rect::from_min_size(rect.min, Vec2::splat(border_size * 2.0)), egui::ResizeDirection::NorthWest),
                (Rect::from_min_size(Pos2::new(rect.max.x - border_size * 2.0, rect.min.y), Vec2::splat(border_size * 2.0)), egui::ResizeDirection::NorthEast),
                (Rect::from_min_size(Pos2::new(rect.min.x, rect.max.y - border_size * 2.0), Vec2::splat(border_size * 2.0)), egui::ResizeDirection::SouthWest),
                (Rect::from_min_size(Pos2::new(rect.max.x - border_size * 2.0, rect.max.y - border_size * 2.0), Vec2::splat(border_size * 2.0)), egui::ResizeDirection::SouthEast),
            ];

            egui::Area::new(egui::Id::new("window_resize_overlay_area"))
                .fixed_pos(Pos2::ZERO)
                .order(egui::Order::Foreground)
                .interactable(true)
                .show(ctx, |ui| {
                    for (i, (edge_rect, direction)) in edges.into_iter().enumerate() {
                        let id = ui.id().with(("window_resize_edge", i));
                        let resp = ui.interact(edge_rect, id, egui::Sense::drag());

                        if resp.hovered() || resp.dragged() {
                            let cursor = match direction {
                                egui::ResizeDirection::North | egui::ResizeDirection::South => egui::CursorIcon::ResizeVertical,
                                egui::ResizeDirection::East | egui::ResizeDirection::West => egui::CursorIcon::ResizeHorizontal,
                                egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => egui::CursorIcon::ResizeNwSe,
                                egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => egui::CursorIcon::ResizeNeSw,
                            };
                            ctx.set_cursor_icon(cursor);
                        }

                        if resp.drag_started() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
                        }
                    }
                });
        }

        let mut st = self.state.write().unwrap();
        st.connected_audio = self.audio_connected.load(Ordering::Relaxed);
        st.connected_ws = self.ws_connected.load(Ordering::Relaxed);

        // Around line 536:
        if let Some(art_id) = st.current_artist_id.clone() {
            if !st.artist_info_cache.contains_key(&art_id)
                && !st.pending_artist_fetches.contains(&art_id)
            {
                st.pending_artist_fetches.insert(art_id.clone());
                let _ = self.spotify_req_tx.try_send(SpotifyRequest::FetchArtistBio(art_id));
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(COLOR_BG).inner_margin(8.0))
            .show(ctx, |ui| {
                // --- 1. Background Drag & Maximize Area for Empty Space ---
                let header_h = 32.0_f32;
                let header_rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), header_h));

                // Sense drags on the background without blocking child clicks
                let header_resp = ui.interact(header_rect, ui.id().with("top_bar_drag"), egui::Sense::drag());
                if header_resp.drag_started() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if header_resp.double_clicked() {
                    let is_maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                }

                // --- 2. Top Header Elements ---
                ui.horizontal(|ui| {
                    // --- Custom Window Control Dots ---
                    let dot_radius = 5.0_f32;
                    let dot_size = Vec2::splat(dot_radius * 2.0);

                    // 1. Red (Close)
                    let (red_rect, red_resp) = ui.allocate_exact_size(dot_size, egui::Sense::click());
                    let red_hovered = red_resp.hovered();
                    let red_col = if red_hovered {
                        Color32::from_rgb(255, 120, 110)
                    } else {
                        Color32::from_rgb(255, 95, 87)
                    };
                    ui.painter().circle_filled(red_rect.center(), dot_radius, red_col);
                    if red_resp.clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }

                    ui.add_space(5.0);

                    // 2. Yellow (Minimize)
                    let (yellow_rect, yellow_resp) = ui.allocate_exact_size(dot_size, egui::Sense::click());
                    let yellow_hovered = yellow_resp.hovered();
                    let yellow_col = if yellow_hovered {
                        Color32::from_rgb(255, 215, 100)
                    } else {
                        Color32::from_rgb(254, 188, 46)
                    };
                    ui.painter().circle_filled(yellow_rect.center(), dot_radius, yellow_col);

                    if yellow_resp.clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        ui.ctx().request_repaint();
                    }

                    ui.add_space(5.0);

                    // 3. Green (Maximize / Restore Toggle)
                    let (green_rect, green_resp) = ui.allocate_exact_size(dot_size, egui::Sense::click());
                    let green_hovered = green_resp.hovered();
                    let green_col = if green_hovered {
                        Color32::from_rgb(80, 235, 115)
                    } else {
                        Color32::from_rgb(40, 200, 64)
                    };
                    ui.painter().circle_filled(green_rect.center(), dot_radius, green_col);
                    if green_resp.clicked() {
                        let is_maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                    }

                    ui.add_space(10.0);

                    // --- App Title Text ---
                    ui.label(
                        RichText::new("spotify-tui")
                            .font(FontId::monospace(11.5))
                            .color(COLOR_TEXT_DIM),
                    );

                    ui.add_space(20.0);

                    let search_box_h = 32.0;
                    let search_bar_width = (ui.available_width() - 260.0).clamp(140.0, 460.0);

                    // --- Back & Forward Navigation Buttons ---
                    let can_back = st.can_go_back();
                    let can_forward = st.can_go_forward();

                    let btn_back = egui::Button::new(
                        RichText::new("◀").font(FontId::monospace(11.0)).color(if can_back { COLOR_TEXT_BRIGHT } else { COLOR_BORDER })
                    )
                        .fill(Color32::from_rgb(22, 22, 26))
                        .stroke(Stroke::new(1.0_f32, COLOR_BORDER))
                        .rounding(Rounding::ZERO)
                        .min_size(Vec2::new(24.0, search_box_h - 4.0));

                    if ui.add_enabled(can_back, btn_back).clicked() {
                        if let Some(target) = st.go_back() {
                            dispatch_nav(target, &mut st, &self.spotify_req_tx);
                        }
                    }

                    let btn_fwd = egui::Button::new(
                        RichText::new("▶").font(FontId::monospace(11.0)).color(if can_forward { COLOR_TEXT_BRIGHT } else { COLOR_BORDER })
                    )
                        .fill(Color32::from_rgb(22, 22, 26))
                        .stroke(Stroke::new(1.0_f32, COLOR_BORDER))
                        .rounding(Rounding::ZERO)
                        .min_size(Vec2::new(24.0, search_box_h - 4.0));

                    if ui.add_enabled(can_forward, btn_fwd).clicked() {
                        if let Some(target) = st.go_forward() {
                            dispatch_nav(target, &mut st, &self.spotify_req_tx);
                        }
                    }

                    ui.add_space(10.0);

                    let is_search_hovered = ui.input(|i| {
                        i.pointer.hover_pos().map_or(false, |pos| {
                            let cur_cursor = ui.cursor();
                            let search_rect = Rect::from_min_size(cur_cursor.min, Vec2::new(search_bar_width, 32.0));
                            search_rect.contains(pos)
                        })
                    });

                    let search_border_color = if is_search_hovered {
                        Color32::from_rgb(140, 145, 175)
                    } else if !st.search_query.is_empty() {
                        COLOR_ACCENT_ROSE
                    } else {
                        COLOR_BORDER
                    };

                    let search_frame = egui::Frame::none()
                        .stroke(Stroke::new(1.0_f32, search_border_color))
                        .rounding(Rounding::ZERO)
                        .fill(COLOR_BG)
                        .inner_margin(egui::Margin::symmetric(10.0, 4.0));

                    search_frame.show(ui, |ui| {
                        ui.set_width(search_bar_width);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("🔍").size(12.0).color(if is_search_hovered { COLOR_TEXT_BRIGHT } else { COLOR_TEXT_DIM }));
                            let edit = ui.add(
                                egui::TextEdit::singleline(&mut st.search_query)
                                    .hint_text("What do you want to play? (Press Enter)")
                                    .desired_width((search_bar_width - 45.0).max(50.0))
                                    .frame(false)
                                    .font(FontId::monospace(12.0))
                                    .text_color(COLOR_TEXT_BRIGHT),
                            );

                            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                let query = st.search_query.trim().to_string();
                                if !query.is_empty() {
                                    st.navigate_to(NavLocation::Search { query: query.clone() });
                                    let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::Search(query));
                                }
                            }
                        });
                    });

                    // User Profile (Right-aligned)
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(6.0);

                        let name = st.user_name.as_deref().unwrap_or("Spotify");
                        let prof_resp = ui.add(
                            egui::Button::new(
                                RichText::new(name)
                                    .font(FontId::monospace(11.5))
                                    .color(COLOR_TEXT_BRIGHT),
                            )
                                .fill(Color32::TRANSPARENT)
                                .frame(false),
                        );

                        ui.add_space(4.0);

                        let avatar_size = Vec2::splat(18.0);
                        if let Some(ref avatar_url) = st.user_avatar_url {
                            ui.add(
                                egui::Image::new(avatar_url)
                                    .fit_to_exact_size(avatar_size)
                                    .rounding(Rounding::same(9.0)),
                            );
                        } else {
                            let (rect, _) = ui.allocate_exact_size(avatar_size, egui::Sense::hover());
                            ui.painter().circle_filled(rect.center(), 9.0, Color32::from_rgb(45, 48, 66));
                            ui.painter().text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "👤",
                                FontId::monospace(10.0),
                                COLOR_TEXT_DIM,
                            );
                        }

                        // Clicking on the profile name or placeholder allows re-logging in
                        if prof_resp.on_hover_text("Click to log in or switch Spotify account").clicked() {
                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::Reauthenticate);
                        }
                    });
                });

                ui.add_space(6.0);

                // Replace the column width clamping logic in update():
                let total_w = ui.available_width().max(0.0);
                // Ensure body_height cannot be negative even during resize or minimize transitions
                let bottom_panel_h = 82.0_f32;
                let row_gap = 6.0_f32;

                // Reserve exact space so the bottom box hits the 8.0px window margin
                let body_height = (ui.available_height() - bottom_panel_h - row_gap).max(0.0);

                let item_spacing_x = ui.spacing().item_spacing.x;
                let splitter_w = 4.0_f32;

                let min_col_w = 140.0_f32;
                let max_col_w = (total_w / 3.0_f32).max(min_col_w);

                let left_w = st.left_col_width.clamp(min_col_w, max_col_w);
                let right_w = st.right_col_width.clamp(min_col_w, max_col_w);

                let fixed_overhead = (splitter_w * 2.0) + (item_spacing_x * 4.0);
                let center_w = (total_w - left_w - right_w - fixed_overhead).max(0.0);

                ui.horizontal(|ui| {
                    // Left Column
                    self.render_tui_box(ui, "Your Library", false, Vec2::new(left_w, body_height), |ui| {
                        egui::ScrollArea::vertical().id_salt("library_scroll_area").show(ui, |ui| {
                            // Provide 4px left indent so button hover padding doesn't touch the clip boundary
                            ui.spacing_mut().item_spacing.y = 2.0;

                            egui::Frame::none()
                                .inner_margin(egui::Margin { left: 4.0, right: 4.0, top: 0.0, bottom: 0.0 })
                                .show(ui, |ui| {
                                    // --- 1. Permanent Top-Level Navigation ---
                                let is_home = st.active_nav == "Home";
                                if ui.selectable_label(
                                    is_home,
                                    RichText::new("🏠 Home")
                                        .font(FontId::monospace(11.5))
                                        .color(if is_home { COLOR_ACCENT_ROSE } else { COLOR_TEXT_BRIGHT }),
                                ).clicked() {
                                    st.navigate_to(NavLocation::Home);
                                    dispatch_nav(NavLocation::Home, &mut st, &self.spotify_req_tx);
                                }

                                ui.add_space(4.0);

                                let is_liked = st.active_nav == "Liked Songs";
                                if ui.selectable_label(
                                    is_liked,
                                    RichText::new("💚 Liked Songs\n   Playlist • Auto")
                                        .font(FontId::monospace(11.5))
                                        .color(if is_liked { COLOR_ACCENT_ROSE } else { COLOR_TEXT_BRIGHT }),
                                ).clicked() {
                                    st.navigate_to(NavLocation::LikedSongs);
                                    dispatch_nav(NavLocation::LikedSongs, &mut st, &self.spotify_req_tx);
                                }

                                ui.add_space(6.0);
                                ui.separator();
                                ui.add_space(6.0);

                                // --- 2. Filter Tabs Under Home & Liked Songs ---
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);

                                    let filters = [
                                        (LibraryFilter::All, "All"),
                                        (LibraryFilter::Playlists, "Playlists"),
                                        (LibraryFilter::Artists, "Artists"),
                                        (LibraryFilter::Albums, "Albums"),
                                    ];

                                    for (flt, label) in filters {
                                        let is_sel = st.library_filter == flt;
                                        let btn = egui::Button::new(
                                            RichText::new(label)
                                                .font(FontId::monospace(10.0))
                                                .color(if is_sel { Color32::BLACK } else { COLOR_TEXT_BRIGHT }),
                                        )
                                            .fill(if is_sel { COLOR_ACCENT_ROSE } else { Color32::from_white_alpha(12) })
                                            .rounding(Rounding::same(2.0));

                                        if ui.add(btn).clicked() {
                                            st.library_filter = flt;
                                        }
                                    }
                                });

                                ui.add_space(6.0);
                                ui.separator();
                                ui.add_space(6.0);

                                let mut nav_action: Option<NavLocation> = None;

                                // --- 3. Playlists List ---
                                if matches!(st.library_filter, LibraryFilter::All | LibraryFilter::Playlists) && !st.user_playlists.is_empty() {
                                    ui.label(RichText::new("PLAYLISTS").font(FontId::monospace(10.0)).color(COLOR_TEXT_DIM));
                                    ui.add_space(2.0);

                                    for pl in &st.user_playlists {
                                        let is_active = st.active_nav == pl.name;
                                        let label = format!("📁 {}\n   {} tracks", pl.name, pl.tracks_total);

                                        if ui.selectable_label(
                                            is_active,
                                            RichText::new(label)
                                                .font(FontId::monospace(11.0))
                                                .color(if is_active { COLOR_ACCENT_ROSE } else { COLOR_TEXT_BRIGHT }),
                                        ).clicked() {
                                            nav_action = Some(NavLocation::Playlist { id: pl.id.clone(), title: pl.name.clone() });
                                        }
                                        ui.add_space(4.0);
                                    }
                                    ui.add_space(4.0);
                                }

                                // --- 4. Followed Artists List ---
                                if matches!(st.library_filter, LibraryFilter::All | LibraryFilter::Artists) && !st.user_followed_artists.is_empty() {
                                    ui.label(RichText::new("ARTISTS").font(FontId::monospace(10.0)).color(COLOR_TEXT_DIM));
                                    ui.add_space(2.0);

                                    for art in &st.user_followed_artists {
                                        let is_active = st.active_nav == art.name;
                                        let label = format!("👤 {}\n   Artist", art.name);

                                        if ui.selectable_label(
                                            is_active,
                                            RichText::new(label)
                                                .font(FontId::monospace(11.0))
                                                .color(if is_active { COLOR_ACCENT_ROSE } else { COLOR_TEXT_BRIGHT }),
                                        ).clicked() {
                                            nav_action = Some(NavLocation::Artist { id: art.id.clone() });
                                        }
                                        ui.add_space(4.0);
                                    }
                                    ui.add_space(4.0);
                                }

                                // --- 5. Saved Albums List ---
                                if matches!(st.library_filter, LibraryFilter::All | LibraryFilter::Albums) && !st.user_saved_albums.is_empty() {
                                    ui.label(RichText::new("ALBUMS").font(FontId::monospace(10.0)).color(COLOR_TEXT_DIM));
                                    ui.add_space(2.0);

                                    for alb in &st.user_saved_albums {
                                        let is_active = st.active_nav == alb.name;
                                        let label = format!("💿 {}\n   {}", alb.name, alb.artist);

                                        if ui.selectable_label(
                                            is_active,
                                            RichText::new(label)
                                                .font(FontId::monospace(11.0))
                                                .color(if is_active { COLOR_ACCENT_ROSE } else { COLOR_TEXT_BRIGHT }),
                                        ).clicked() {
                                            nav_action = Some(NavLocation::Album { id: alb.id.clone(), title: alb.name.clone() });
                                        }
                                        ui.add_space(4.0);
                                    }
                                }

                                    // --- 6. Navigation Dispatch ---
                                    if let Some(target) = nav_action {
                                        st.navigate_to(target.clone());
                                        dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                    }
                                });
                        });
                    });

                    let mut left_width_var = st.left_col_width;
                    render_v_splitter(ui, "split_left", body_height, &mut left_width_var, min_col_w, max_col_w, false);
                    st.left_col_width = left_width_var;

                    // Center Column
                    self.render_tui_box(ui, "Main", false, Vec2::new(center_w, body_height), |ui| {
                        let inner_w = ui.available_width().max(60.0);
                        ui.set_max_width(inner_w);

                        egui::ScrollArea::vertical()
                            .id_salt("center_main_track_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                // Clamp child container strictly to the box width
                                ui.set_max_width(inner_w);
                            if let Some(artist) = st.artist_page.clone() {
                                // 1. HEADER & ABOUT STATS
                                render_dynamic_ascii_banner(ui, &artist.name, COLOR_ACCENT_ROSE);
                                ui.add_space(2.0);
                                ui.horizontal(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(format!(
                                            "👥 {} followers   •   🎧 {} monthly listeners",
                                            artist.followers, artist.monthly_listeners
                                        )).font(FontId::monospace(11.0)).color(COLOR_TEXT_DIM));

                                        let (btn_txt, btn_col, is_enabled) = match st.is_context_saved {
                                            Some(true) => ("[♥] Following", COLOR_ACCENT_ROSE, true),
                                            Some(false) => ("[♡] Follow", COLOR_TEXT_DIM, true),
                                            None => ("[..] Checking", COLOR_MUTED, false),
                                        };

                                        if ui.add_enabled(
                                            is_enabled,
                                            egui::Button::new(RichText::new(btn_txt).font(FontId::monospace(10.5)).color(btn_col))
                                                .fill(Color32::TRANSPARENT),
                                        ).clicked() {
                                            let current = st.is_context_saved.unwrap_or(false);
                                            let artist_uri = if artist.id.starts_with("spotify:artist:") {
                                                artist.id.clone()
                                            } else {
                                                format!("spotify:artist:{}", artist.id)
                                            };
                                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::ToggleLibraryUri {
                                                uri: artist_uri,
                                                current_state: current,
                                                is_context: true,
                                            });
                                        }
                                    });
                                });

                                if !artist.bio.is_empty() {
                                    ui.add_space(6.0);

                                    if st.artist_bio_expanded {
                                        let segments = parse_spotify_html_bio(&artist.bio);

                                        egui::ScrollArea::vertical()
                                            .id_salt("artist_bio_expanded_scroll")
                                            .max_height(180.0)
                                            .show(ui, |ui| {
                                                self.render_bio_segments(ui, &mut *st, &segments);
                                            });

                                        ui.add_space(4.0);
                                        if ui.add(
                                            egui::Button::new(
                                                RichText::new("[show less]")
                                                    .font(FontId::monospace(10.5))
                                                    .color(COLOR_ACCENT_ROSE),
                                            )
                                                .fill(Color32::TRANSPARENT),
                                        ).clicked() {
                                            st.artist_bio_expanded = false;
                                        }
                                    } else {
                                        // Remove the remaining href uri and tag closes using a clean pass
                                        let mut plain_preview = String::new();
                                        let mut in_tag = false;
                                        for ch in artist.bio.chars() {
                                            if ch == '<' {
                                                in_tag = true;
                                            } else if ch == '>' {
                                                in_tag = false;
                                            } else if !in_tag {
                                                plain_preview.push(ch);
                                            }
                                        }

                                        let font_id = FontId::monospace(11.0);
                                        let text_color = COLOR_TEXT_BRIGHT;
                                        let wrap_width = ui.available_width().max(100.0);

                                        let full_galley = ui.painter().layout(
                                            plain_preview.clone(),
                                            font_id.clone(),
                                            text_color,
                                            wrap_width,
                                        );

                                        if full_galley.rows.len() <= 2 {
                                            ui.label(RichText::new(&plain_preview).font(font_id).italics().color(text_color));
                                        } else {
                                            let preview_text: String = full_galley.rows[..2]
                                                .iter()
                                                .flat_map(|r| r.glyphs.iter().map(|g| g.chr))
                                                .collect::<String>()
                                                .trim_end()
                                                .to_string() + "...";

                                            ui.horizontal_wrapped(|ui| {
                                                ui.label(RichText::new(preview_text).font(font_id).italics().color(text_color));
                                                if ui.add(
                                                    egui::Button::new(
                                                        RichText::new("[read more...]")
                                                            .font(FontId::monospace(10.5))
                                                            .color(COLOR_ACCENT_ROSE),
                                                    )
                                                        .fill(Color32::TRANSPARENT),
                                                ).clicked() {
                                                    st.artist_bio_expanded = true;
                                                }
                                            });
                                        }
                                    }
                                }

                                if !artist.top_cities.is_empty() {
                                    ui.add_space(4.0);
                                    let city_str = artist.top_cities.iter()
                                        .take(4)
                                        .map(|(city, country, count)| format!("{}/{} ({}k)", city, country, count / 1000))
                                        .collect::<Vec<_>>()
                                        .join(" • ");
                                    ui.label(RichText::new(format!("Top Cities: {}", city_str)).font(FontId::monospace(10.0)).color(COLOR_TEXT_DIM));
                                }

                                ui.add_space(10.0);
                                ui.separator();

                                // 2. TOP TRACKS
                                if !artist.top_tracks.is_empty() {
                                    ui.add_space(4.0);
                                    ui.label(RichText::new("POPULAR TRACKS").font(FontId::monospace(12.0)).strong().color(COLOR_ACCENT_ROSE));
                                    ui.add_space(2.0);

                                    for (i, track) in artist.top_tracks.iter().enumerate() {
                                        let is_sel = st.selected_track_uri.as_ref() == Some(&track.uri);
                                        let dur_m = track.duration_ms / 60000;
                                        let dur_s = (track.duration_ms / 1000) % 60;
                                        let line = format!("{:02}. {} [{}] ({:02}:{:02})", i + 1, track.title, track.album, dur_m, dur_s);

                                        // In Artist Top Tracks section:
                                        if ui.selectable_label(
                                            is_sel,
                                            RichText::new(line)
                                                .font(FontId::monospace(11.5))
                                                .color(if is_sel { Color32::BLACK } else { COLOR_TEXT_BRIGHT }),
                                        ).clicked() {
                                            st.selected_track_uri = Some(track.uri.clone());

                                            // Slice upcoming tracks from the artist's popular list
                                            let upcoming: Vec<String> = artist.top_tracks[i..]
                                                .iter()
                                                .map(|t| t.uri.clone())
                                                .collect();

                                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::StartPlayback {
                                                uri: track.uri.clone(),
                                                context_uri: None,
                                                upcoming_uris: upcoming,
                                            });
                                        }
                                    }
                                }

                                // 3. DISCOGRAPHY: ALBUMS
                                if !artist.albums.is_empty() {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.label(RichText::new("ALBUMS").font(FontId::monospace(12.0)).strong().color(COLOR_ACCENT_ROSE));
                                    ui.add_space(2.0);

                                    for album in &artist.albums {
                                        let year_str = album.year.map(|y| format!("{}", y)).unwrap_or_else(|| "-".into());
                                        let line = format!("💿 {} ({}) • {} tracks", album.name, year_str, album.track_count);
                                        if ui.button(RichText::new(line).font(FontId::monospace(11.0)).color(COLOR_TEXT_BRIGHT)).clicked() {
                                            let id = album.uri.trim_start_matches("spotify:album:").to_string();
                                            st.view_title = format!("Album: {}", album.name);
                                            st.active_nav = album.name.clone();
                                            st.navigate_to(NavLocation::Album { id: id.clone(), title: album.name.clone() });
                                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::FetchAlbum(id));
                                        }
                                    }
                                }

                                // 4. SINGLES & EPS
                                if !artist.singles.is_empty() {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.label(RichText::new("SINGLES & EPS").font(FontId::monospace(12.0)).strong().color(COLOR_ACCENT_ROSE));
                                    ui.add_space(2.0);

                                    for single in &artist.singles {
                                        let year_str = single.year.map(|y| format!("{}", y)).unwrap_or_else(|| "-".into());
                                        let line = format!("🎵 {} ({})", single.name, year_str);
                                        if ui.button(RichText::new(line).font(FontId::monospace(11.0)).color(COLOR_TEXT_BRIGHT)).clicked() {
                                            let id = single.uri.trim_start_matches("spotify:album:").to_string();
                                            st.navigate_to(NavLocation::Album { id: id.clone(), title: single.name.clone() });
                                            st.view_title = format!("Single: {}", single.name);
                                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::FetchAlbum(id));
                                        }
                                    }
                                }

                                // 5. ON TOUR / CONCERTS
                                if !artist.concerts.is_empty() {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.label(RichText::new("ON TOUR").font(FontId::monospace(12.0)).strong().color(Color32::from_rgb(255, 180, 80)));
                                    ui.add_space(2.0);

                                    for concert in &artist.concerts {
                                        let date_str = concert.date.chars().take(10).collect::<String>();
                                        let line = format!("📍 {} • {}, {} ({})", date_str, concert.venue, concert.city, concert.title);
                                        ui.label(RichText::new(line).font(FontId::monospace(10.5)).color(COLOR_TEXT_BRIGHT));
                                    }
                                }

                                // 6. ARTIST PLAYLISTS & FEATURING
                                if !artist.artist_playlists.is_empty() || !artist.featuring_playlists.is_empty() {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.label(RichText::new("PLAYLISTS & FEATURING").font(FontId::monospace(12.0)).strong().color(Color32::from_rgb(200, 120, 240)));
                                    ui.add_space(2.0);

                                    for pl in artist.artist_playlists.iter().chain(artist.featuring_playlists.iter()) {
                                        if ui.button(RichText::new(format!("📁 {}", pl.name)).font(FontId::monospace(11.0)).color(COLOR_TEXT_BRIGHT)).clicked() {
                                            let id = pl.uri.trim_start_matches("spotify:playlist:").to_string();
                                            st.navigate_to(NavLocation::Playlist { id: id.clone(), title: pl.name.clone() });
                                            st.view_title = format!("Playlist: {}", pl.name);
                                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::FetchPlaylist(id));
                                        }
                                    }
                                }

                                // 7. FANS ALSO LIKE (RELATED ARTISTS)
                                if !artist.related_artists.is_empty() {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.label(RichText::new("FANS ALSO LIKE").font(FontId::monospace(12.0)).strong().color(Color32::from_rgb(100, 180, 255)));
                                    ui.add_space(2.0);

                                    ui.horizontal_wrapped(|ui| {
                                        for (rel_name, rel_uri) in &artist.related_artists {
                                            if ui.button(RichText::new(format!("👤 {}", rel_name)).font(FontId::monospace(11.0))).clicked() {
                                                let id = rel_uri.trim_start_matches("spotify:artist:").to_string();
                                                let target = NavLocation::Artist { id };
                                                st.navigate_to(target.clone());
                                                dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                            }
                                        }
                                    });
                                }

                                // 8. DISCOVERED ON
                                if !artist.discovered_on.is_empty() {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.label(RichText::new("DISCOVERED ON").font(FontId::monospace(12.0)).strong().color(Color32::from_rgb(180, 140, 255)));
                                    ui.add_space(2.0);

                                    for pl in &artist.discovered_on {
                                        if ui.button(RichText::new(format!("📻 {}", pl.name)).font(FontId::monospace(11.0)).color(COLOR_TEXT_BRIGHT)).clicked() {
                                            let id = pl.uri.trim_start_matches("spotify:playlist:").to_string();
                                            st.view_title = format!("Playlist: {}", pl.name);
                                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::FetchPlaylist(id));
                                        }
                                    }
                                }

                                // 9. APPEARS ON
                                if !artist.appears_on.is_empty() {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.label(RichText::new("APPEARS ON").font(FontId::monospace(12.0)).strong().color(COLOR_TEXT_DIM));
                                    ui.add_space(2.0);

                                    for app in &artist.appears_on {
                                        let year_str = app.year.map(|y| format!("{}", y)).unwrap_or_else(|| "-".into());
                                        let line = format!("💿 {} ({})", app.name, year_str);
                                        if ui.button(RichText::new(line).font(FontId::monospace(10.5)).color(COLOR_TEXT_DIM)).clicked() {
                                            let id = app.uri.trim_start_matches("spotify:album:").to_string();
                                            st.view_title = format!("Album: {}", app.name);
                                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::FetchAlbum(id));
                                        }
                                    }
                                }
                            } else if !st.search_results.is_empty() {
                                let row_w = (ui.available_width() - 8.0).max(80.0);
                                let badge_w = 72.0_f32;
                                let row_h = 22.0_f32;

                                for item in st.search_results.clone() {
                                    let (badge, badge_color) = match item.kind {
                                        SearchResultKind::Artist => ("[ARTIST]  ", Color32::from_rgb(100, 180, 255)),
                                            SearchResultKind::Album => ("[ALBUM]   ", Color32::from_rgb(255, 180, 80)),
                                            SearchResultKind::Playlist => ("[PLAYLIST]", Color32::from_rgb(200, 120, 240)),
                                            SearchResultKind::Track => ("[TRACK]   ", COLOR_ACCENT_ROSE),
                                    };

                                    let is_sel = st.selected_track_uri.as_ref() == Some(&item.uri);
                                    let line = format!("{}  ({})", item.title, item.subtitle);

                                    let (row_rect, row_resp) = ui.allocate_exact_size(Vec2::new(row_w, row_h), egui::Sense::click());

                                    // Background selection/hover highlight
                                    if is_sel {
                                        ui.painter().rect_filled(row_rect, Rounding::ZERO, Color32::from_white_alpha(18));
                                    } else if row_resp.hovered() {
                                        ui.painter().rect_filled(row_rect, Rounding::ZERO, Color32::from_white_alpha(8));
                                    }

                                    // Render Badge and Truncated Label strictly within row_rect
                                    ui.allocate_new_ui(
                                        egui::UiBuilder::new().max_rect(row_rect),
                                        |ui| {
                                            ui.horizontal(|ui| {
                                                ui.spacing_mut().item_spacing = Vec2::new(6.0, 0.0);

                                                // Fixed Badge
                                                ui.allocate_ui_with_layout(
                                                    Vec2::new(badge_w, row_h),
                                                    egui::Layout::left_to_right(egui::Align::Center),
                                                    |ui| {
                                                        ui.label(RichText::new(badge).font(FontId::monospace(10.5)).color(badge_color));
                                                    },
                                                );

                                                // Truncated Text Title & Subtitle
                                                let text_color = if is_sel { COLOR_ACCENT_ROSE } else { COLOR_TEXT_BRIGHT };
                                                let text_w = (row_w - badge_w - 12.0).max(40.0);
                                                ui.allocate_ui_with_layout(
                                                    Vec2::new(text_w, row_h),
                                                    egui::Layout::left_to_right(egui::Align::Center),
                                                    |ui| {
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(line)
                                                                    .font(FontId::monospace(11.5))
                                                                    .color(text_color),
                                                            )
                                                                .truncate(),
                                                        );
                                                    },
                                                );
                                            });
                                        },
                                    );

                                    if row_resp.clicked() {
                                        match item.kind {
                                            SearchResultKind::Track => {
                                                st.selected_track_uri = Some(item.uri.clone());
                                                let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::StartPlayback {
                                                    uri: item.uri.clone(),
                                                    context_uri: None,
                                                    upcoming_uris: vec![item.uri.clone()],
                                                });
                                                let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::CheckLibraryTrack(item.uri));
                                            }
                                            SearchResultKind::Playlist => {
                                                let id = item.uri.trim_start_matches("spotify:playlist:").to_string();
                                                let target = NavLocation::Playlist { id, title: item.title };
                                                st.navigate_to(target.clone());
                                                dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                            }
                                            SearchResultKind::Album => {
                                                let id = item.uri.trim_start_matches("spotify:album:").to_string();
                                                let target = NavLocation::Album { id, title: item.title };
                                                st.navigate_to(target.clone());
                                                dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                            }
                                            SearchResultKind::Artist => {
                                                let id = item.uri.trim_start_matches("spotify:artist:").to_string();
                                                let target = NavLocation::Artist { id };
                                                st.navigate_to(target.clone());
                                                dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                            }
                                        }
                                    }

                                    ui.add_space(2.0);
                                }
                            } else if !st.displayed_tracks.is_empty() {
                                // 1. Calculate Bounded Column Widths
                                let row_w = (ui.available_width() - 20.0).max(80.0);
                                let show_album = row_w >= 460.0;

                                let col_num_x = 0.0_f32;
                                let col_num_w = 26.0_f32;
                                let col_title_x = 32.0_f32;
                                let dur_w = 44.0_f32;

                                let col_dur_x = row_w - dur_w;
                                let album_w = if show_album {
                                    (row_w * 0.28).clamp(120.0, 260.0)
                                } else {
                                    0.0_f32
                                };

                                let col_album_x = if show_album { col_dur_x - album_w - 12.0 } else { row_w };
                                let col_title_w = if show_album {
                                    (col_album_x - col_title_x - 12.0).max(40.0)
                                } else {
                                    (col_dur_x - col_title_x - 12.0).max(40.0)
                                };

                                // 2. Compute Playlist Stats
                                let count = st.playlist_total_tracks.unwrap_or(st.displayed_tracks.len());
                                let total_ms: u64 = st.displayed_tracks.iter().map(|t| t.duration_ms).sum();
                                let total_mins = total_ms / 60_000;
                                let dur_str = if total_mins >= 60 {
                                    format!("{} hr {} min", total_mins / 60, total_mins % 60)
                                } else {
                                    format!("{} min", total_mins)
                                };

                                let stats_line = if let Some(tot) = st.playlist_total_tracks {
                                    if st.displayed_tracks.len() < tot {
                                        format!("📁 {} tracks (streaming {}/{}...)   •   ⏱ {}", tot, st.displayed_tracks.len(), tot, dur_str)
                                    } else {
                                        format!("📁 {} tracks   •   ⏱ {}", count, dur_str)
                                    }
                                } else {
                                    format!("📁 {} tracks   •   ⏱ {}", count, dur_str)
                                };

                                let mut play_action: Option<(usize, SpotifyTrackItem)> = None;
                                let mut nav_action: Option<NavLocation> = None;
                                let row_h = 33.0_f32;
                                let header_h = 20.0_f32;
                                let total_tracks = st.displayed_tracks.len();

                                let box_top_min = ui.cursor().min;
                                let mut inline_header_y: Option<f32> = None;

                                // Helper closure to draw column headers cleanly
                                let draw_header_items = |ui: &mut egui::Ui, origin: Pos2| {
                                    let h_rect = Rect::from_min_size(origin, Vec2::new(row_w, header_h));

                                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(h_rect.min.x + col_num_x, h_rect.min.y), Vec2::new(col_num_w, header_h))), |ui| {
                                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                            ui.label(RichText::new("#").font(FontId::monospace(10.0)).color(COLOR_MUTED));
                                        });
                                    });
                                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(h_rect.min.x + col_title_x, h_rect.min.y), Vec2::new(col_title_w, header_h))), |ui| {
                                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                            ui.label(RichText::new("TITLE").font(FontId::monospace(10.0)).color(COLOR_MUTED));
                                        });
                                    });
                                    if show_album {
                                        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(h_rect.min.x + col_album_x, h_rect.min.y), Vec2::new(album_w, header_h))), |ui| {
                                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                                ui.label(RichText::new("ALBUM").font(FontId::monospace(10.0)).color(COLOR_MUTED));
                                            });
                                        });
                                    }
                                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(h_rect.min.x + col_dur_x, h_rect.min.y), Vec2::new(dur_w, header_h))), |ui| {
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(RichText::new("⏱").font(FontId::monospace(10.0)).color(COLOR_MUTED));
                                        });
                                    });

                                    ui.painter().line_segment(
                                        [Pos2::new(h_rect.min.x, h_rect.max.y), Pos2::new(h_rect.min.x + row_w, h_rect.max.y)],
                                        Stroke::new(1.0_f32, COLOR_BORDER),
                                    );
                                };

                                // 3. The Unified Scroll Area
                                egui::ScrollArea::vertical()
                                    .id_salt("playlist_content_scroll")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.set_width(row_w);

                                        // A. Banner & Stats (Scroll up naturally)
                                        render_dynamic_ascii_banner(ui, &st.view_title, COLOR_ACCENT_ROSE);
                                        ui.add_space(4.0);

                                        ui.horizontal(|ui| {
                                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                                            let max_stats_w = (ui.available_width() - 80.0).max(60.0);

                                            ui.allocate_ui_with_layout(
                                                Vec2::new(max_stats_w, 18.0),
                                                egui::Layout::left_to_right(egui::Align::Center),
                                                |ui| {
                                                    ui.add(egui::Label::new(
                                                        RichText::new(&stats_line)
                                                            .font(FontId::monospace(11.0))
                                                            .color(COLOR_TEXT_DIM),
                                                    ).truncate());
                                                },
                                            );

                                            if st.active_context_uri.is_some() {
                                                if st.is_context_owner {
                                                    ui.label(RichText::new("[Owner]").font(FontId::monospace(10.5)).color(COLOR_SUBTLE));
                                                } else {
                                                    let (btn_txt, btn_color, is_enabled) = match st.is_context_saved {
                                                        Some(true) => ("[♥] Saved", COLOR_ACCENT_ROSE, true),
                                                        Some(false) => ("[♡] Save", COLOR_TEXT_DIM, true),
                                                        None => ("[..] Checking", COLOR_MUTED, false),
                                                    };

                                                    let follow_btn = egui::Button::new(
                                                        RichText::new(btn_txt).font(FontId::monospace(10.5)).color(btn_color),
                                                    ).fill(Color32::TRANSPARENT);

                                                    if ui.add_enabled(is_enabled, follow_btn).clicked() {
                                                        let current = st.is_context_saved.unwrap_or(false);
                                                        if let Some(ref uri) = st.active_context_uri {
                                                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::ToggleLibraryUri {
                                                                uri: uri.clone(),
                                                                current_state: current,
                                                                is_context: true,
                                                            });
                                                        }
                                                    }
                                                }
                                            }
                                        });

                                        ui.add_space(8.0);
                                        ui.separator();
                                        ui.add_space(4.0);

                                        // Record header position in screen coords
                                        let current_header_pos = ui.cursor().min;
                                        inline_header_y = Some(current_header_pos.y);

                                        // Draw inline column headers when not yet pinned
                                        if current_header_pos.y > box_top_min.y {
                                            draw_header_items(ui, current_header_pos);
                                        } else {
                                            // Reserve empty space so track rows don't jump
                                            ui.allocate_exact_size(Vec2::new(row_w, header_h), egui::Sense::hover());
                                        }

                                        // Restrict track row rendering strictly below the docked header zone
                                        if current_header_pos.y <= box_top_min.y {
                                            let mut track_clip = ui.clip_rect();
                                            track_clip.min.y = box_top_min.y + header_h;
                                            ui.set_clip_rect(track_clip);
                                        }

                                        ui.add_space(6.0);

                                        // B. Virtualized Track List via show_rows (Lightweight & 60 FPS)
                                        ui.push_id("track_rows_virtualized", |ui| {

                                            // Calculate visible slice manually or render with slice inside viewport
                                            let scroll_y = ui.clip_rect().min.y - ui.min_rect().min.y;
                                            let start_idx = (scroll_y / row_h).floor().max(0.0) as usize;
                                            let num_visible = (ui.clip_rect().height() / row_h).ceil() as usize + 2;
                                            let end_idx = (start_idx + num_visible).min(total_tracks);

                                            // Top spacer for virtual items above viewport
                                            if start_idx > 0 {
                                                ui.add_space(start_idx as f32 * row_h);
                                            }

                                            let mut context_menu_action: Option<TrackContextMenu> = None;

                                            for i in start_idx..end_idx {
                                                let track = &st.displayed_tracks[i];
                                                let is_sel = st.selected_track_uri.as_ref() == Some(&track.uri);
                                                let dur_s = (track.duration_ms / 1000) % 60;
                                                let dur_m = track.duration_ms / 60000;
                                                let time_str = format!("{:02}:{:02}", dur_m, dur_s);

                                                let (row_rect, row_resp) = ui.allocate_exact_size(Vec2::new(row_w, row_h), egui::Sense::click());

                                                if row_resp.clicked() {
                                                    play_action = Some((i, track.clone()));
                                                }

                                                // Capture right-click to spawn context menu
                                                if row_resp.secondary_clicked() {
                                                    let mouse_pos = ui.input(|i| i.pointer.interact_pos())
                                                        .or_else(|| ui.input(|i| i.pointer.hover_pos()))
                                                        .unwrap_or(row_rect.left_bottom());

                                                    // Fast check against the local liked cache
                                                    let is_saved = st.liked_track_uris.contains(&track.uri)
                                                        || st.active_nav == "Liked Songs"
                                                        || (st.selected_track_uri.as_ref() == Some(&track.uri) && st.is_track_saved);

                                                    // Trigger API verification if not currently confirmed
                                                    if !is_saved {
                                                        let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::CheckLibraryTrack(track.uri.clone()));
                                                    }

                                                    context_menu_action = Some(TrackContextMenu {
                                                        track: track.clone(),
                                                        click_pos: mouse_pos,
                                                        filter_query: String::new(),
                                                        show_playlist_submenu: false,
                                                        is_saved,
                                                        track_index: i,
                                                    });
                                                }
                                                
                                                if is_sel {
                                                    ui.painter().rect_filled(row_rect, Rounding::ZERO, Color32::from_white_alpha(18));
                                                    ui.painter().rect_stroke(row_rect, Rounding::ZERO, Stroke::new(1.0_f32, COLOR_ACCENT_ROSE));
                                                } else if row_resp.hovered() {
                                                    ui.painter().rect_filled(row_rect, Rounding::ZERO, Color32::from_white_alpha(8));
                                                    ui.painter().rect_stroke(row_rect, Rounding::ZERO, ui.visuals().widgets.hovered.bg_stroke);
                                                }

                                                if row_resp.clicked() {
                                                    play_action = Some((i, track.clone()));
                                                }

                                                // Cell 1: Track Number
                                                ui.allocate_new_ui(
                                                    egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(row_rect.min.x + col_num_x, row_rect.min.y), Vec2::new(col_num_w, row_h))),
                                                    |ui| {
                                                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                                            let num_col = if is_sel { COLOR_ACCENT_ROSE } else { COLOR_MUTED };
                                                            ui.label(RichText::new(format!("{:02}", i + 1)).font(FontId::monospace(10.5)).color(num_col));
                                                        });
                                                    },
                                                );

                                                // Cell 2: Title & Multi-Artist Links
                                                ui.allocate_new_ui(
                                                    egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(row_rect.min.x + col_title_x, row_rect.min.y), Vec2::new(col_title_w, row_h))),
                                                    |ui| {
                                                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                                            ui.add_space(2.0);
                                                            let title_color = if is_sel { COLOR_ACCENT_ROSE } else { COLOR_TEXT_BRIGHT };
                                                            ui.add(egui::Label::new(RichText::new(&track.title).font(FontId::monospace(11.5)).strong().color(title_color)).truncate());

                                                            if !track.artists.is_empty() {
                                                                Self::render_artist_links(
                                                                    ui,
                                                                    &track.artists,
                                                                    COLOR_TEXT_DIM,
                                                                    FontId::monospace(10.0),
                                                                    &mut nav_action,
                                                                );
                                                            } else {
                                                                let single_artist = vec![(track.artist.clone(), String::new())];
                                                                Self::render_artist_links(
                                                                    ui,
                                                                    &single_artist,
                                                                    COLOR_TEXT_DIM,
                                                                    FontId::monospace(10.0),
                                                                    &mut nav_action,
                                                                );
                                                            }
                                                        });
                                                    },
                                                );

                                                // Cell 3: Album Link
                                                if show_album {
                                                    ui.allocate_new_ui(
                                                        egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(row_rect.min.x + col_album_x, row_rect.min.y), Vec2::new(album_w, row_h))),
                                                        |ui| {
                                                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                                                let alb_resp = Self::render_inline_link(
                                                                    ui,
                                                                    &track.album,
                                                                    COLOR_MUTED,
                                                                    FontId::monospace(10.5),
                                                                    album_w,
                                                                );

                                                                if alb_resp.clicked() {
                                                                    if let Some(ref al_id) = track.album_id {
                                                                        nav_action = Some(NavLocation::Album {
                                                                            id: al_id.clone(),
                                                                            title: track.album.clone(),
                                                                        });
                                                                    }
                                                                }
                                                            });
                                                        },
                                                    );
                                                }

                                                // Cell 4: Duration
                                                ui.allocate_new_ui(
                                                    egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(row_rect.min.x + col_dur_x, row_rect.min.y), Vec2::new(dur_w, row_h))),
                                                    |ui| {
                                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                            ui.label(RichText::new(time_str).font(FontId::monospace(10.5)).color(COLOR_TEXT_DIM));
                                                        });
                                                    },
                                                );
                                            }

                                            if let Some(menu) = context_menu_action {
                                                st.context_menu = Some(menu);
                                            }
                                            // Bottom spacer for virtual items below viewport
                                            let remaining = total_tracks.saturating_sub(end_idx);
                                            if remaining > 0 {
                                                ui.add_space(remaining as f32 * row_h);
                                            }
                                        });
                                    });

                                // 4. PINNED STICKY HEADER (Renders at top of box only when scrolled off)
                                if let Some(inline_y) = inline_header_y {
                                    if inline_y <= box_top_min.y {
                                        // 1. Mask from the absolute inner top border of the box down through the header
                                        let top_bound = ui.clip_rect().min.y;
                                        let mask_rect = Rect::from_min_max(
                                            Pos2::new(box_top_min.x - 4.0, top_bound),
                                            Pos2::new(box_top_min.x + row_w + 4.0, box_top_min.y + header_h + 2.0),
                                        );
                                        ui.painter().rect_filled(mask_rect, Rounding::ZERO, COLOR_BG);

                                        // 2. Draw the pinned header items
                                        draw_header_items(ui, box_top_min);
                                    }
                                }

                                if let Some(target) = nav_action {
                                    st.navigate_to(target.clone());
                                    dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                }

                                if let Some((i, track)) = play_action {
                                    st.selected_track_uri = Some(track.uri.clone());
                                    let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::CheckLibraryTrack(track.uri.clone()));
                                    st.title = track.title.clone();
                                    st.artist = track.artist.clone();
                                    st.album = track.album.clone();
                                    st.duration_ms = track.duration_ms;
                                    st.position_ms = 0;
                                    st.last_sync = Instant::now();

                                    // Send the full remainder of the track list to populate the context queue
                                    let upcoming: Vec<String> = st.displayed_tracks[i..]
                                        .iter()
                                        .map(|t| t.uri.clone())
                                        .collect();

                                    let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::StartPlayback {
                                        uri: track.uri,
                                        context_uri: st.active_context_uri.clone(),
                                        upcoming_uris: upcoming,
                                    });
                                }
                            } else if st.active_nav != "Home" {
                                // 4. NON-HOME LOADING PLACEHOLDER
                                // Triggers anytime you are on a playlist/album/artist/search,
                                // preventing the ASCII screen from ever flashing.
                                ui.vertical_centered(|ui| {
                                    ui.add_space(60.0);
                                    ui.add(egui::Spinner::new().size(22.0).color(COLOR_ACCENT_ROSE));
                                    ui.add_space(12.0);
                                    ui.label(
                                        RichText::new(format!("Loading {}...", st.view_title))
                                            .font(FontId::monospace(12.0))
                                            .color(COLOR_TEXT_DIM),
                                    );
                                });
                            } else {
                                const HOMEPAGE_ART: &str = r#"


                              ++++****
            =============+++**#######*
          =+++++++++++++++****######**
          +*********************###*++
          +*####******++*#####***##*+
          ++*####%@@@*+++*#@@@%##*#**
           ++*####%%##++***########**
           =+***##*++++****+++*####*+
           ++******+++*****+****##*+
           +***###**********######*+          ======
          +**########****######%%%*++      ========
          +*############%%%%%%%%%%##*++++****++
          +*###%%%%%%%%%%%%%%%%%%%%########***
         +*####%%%%%%%%%%%%%%%%%%%%%#####**
        ++*###%%%%%%%%%%%%%%%%%%%%%%%%###
     ++**####%%%%%%%%%%%%%%%%%%%%%%%%##*
  +++**#####%%%%%%%%%%%%%%%%%%%%%%%%%##
  ++***#####%%%%%%%%%%%%%%%%%%%%%%%%%#
  ++***####%%%%%%%%%%%%%%%%%%%%%%%%%%#
    ++*#####%%%%%%%%%%%%%%%%%%%%%%%%%#
      ++*###%%%%%%%%%%%%%%%%%%%%%%%%%##
       ++**##%%%%%%%%%%%%%%%%%%%%%%%%###
        ++*###%%%%%%%%%%%%%%%%%%%%%%%%##
         +*#####%%%%%%%%%%%%%%%%%%%%%%%##
          *####%%%%%%%%%%%%%%%%%%%%%%%%####
          +*####%%%%%%%%%%%%%%%%%%%%%%%%%##
           +*###%%%%%%%%%%%%%%%%%%%%%%%%%%##
           +*####%%%%%%%%%%%%%%%%%%%%%%%%%%#*
           +*######%%%%%%%%%%%%%%%%%%%%%%%%#**
           +*########%%%%%%%%%%%%%%%%%%%%%%###*
           ++*########%%%%%%%%%%%%%%%%%%%%%%%##***
           ++*#########%%%%%%%%%%%%%%%%%%%%%%%%##***
           ++*#########%%%%%%%%%%%%%%%%%%%%%%%%###***
           ++*#########%%%%%%%%%%%%%%%%%%%%%%%%##***
           ++*##########%##%%%%%%%%%%%%%%%%%%###*
           ++*#################%%%%%%%%%%%%##*
           ++*#######################%%%%%%#*
           ++*###############   ########%##*
           ++*##########*          #######**
           ++*########*             *#####*+
           ++*######**              *####**+
            +***###*                *####**+
            ++******                *####**+
              +*****                *####**+
              +****+                *####*+
              ++*++                 ***##*+
              =+++                  ++****+
               ===                   +++++
                                     +==+

"#;

                                // Measure the art width to keep it perfectly intact and left-aligned within a centered block
                                let font_size = 7.5_f32;
                                let line_h = font_size * 1.35;
                                let font_id = FontId::monospace(font_size);

                                let galley = ui.painter().layout_no_wrap(
                                    HOMEPAGE_ART.to_string(),
                                    font_id.clone(),
                                    COLOR_ACCENT_ROSE,
                                );
                                let art_size = Vec2::new(galley.size().x, HOMEPAGE_ART.lines().count() as f32 * line_h);

                                ui.vertical_centered(|ui| {
                                    ui.add_space(12.0);

                                    // Allocate container with precise line height
                                    ui.allocate_ui_with_layout(
                                        art_size,
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(HOMEPAGE_ART)
                                                        .font(font_id)
                                                        .line_height(Some(line_h))
                                                        .color(COLOR_ACCENT_ROSE),
                                                )
                                                    .wrap_mode(egui::TextWrapMode::Extend),
                                            );
                                        },
                                    );

                                    ui.add_space(4.0);
                                    let gh_url = "https://github.com/simburto/spotify-tui";
                                    let gh_btn = ui.add(
                                        egui::Button::new(
                                            RichText::new("⑂ GitHub: spotify-tui")
                                                .font(FontId::monospace(10.5))
                                                .underline()
                                                .color(COLOR_ACCENT_GREEN),
                                        )
                                            .fill(Color32::TRANSPARENT)
                                            .frame(false),
                                    );

                                    if gh_btn.hovered() {
                                        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                                    }
                                    if gh_btn.clicked() {
                                        let _ = open::that(gh_url);
                                    }
                                });
                            }
                        });
                    });

                    // Splitter between Center and Right
                    let mut right_width_var = st.right_col_width;
                    render_v_splitter(ui, "split_right", body_height, &mut right_width_var, min_col_w, max_col_w, true);
                    st.right_col_width = right_width_var;

                    let right_title = if st.show_queue {
                        "Queue".to_string()
                    } else {
                        "Now Playing".to_string()
                    };

                    let show_queue = st.show_queue;

                    self.render_tui_box(ui, &right_title, show_queue, Vec2::new(right_w, body_height), |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("now_playing_right_scroll_area")
                            .show(ui, |ui| {
                                ui.set_max_width(right_w - 20.0);
                                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                                if st.show_queue {
                                    // 1. NOW PLAYING
                                    ui.label(
                                        RichText::new("Now playing")
                                            .font(FontId::monospace(11.0))
                                            .strong()
                                            .color(COLOR_TEXT_BRIGHT),
                                    );
                                    ui.add_space(4.0);

                                    if st.title == "Waiting for playback..." || st.title.is_empty() {
                                        ui.label(
                                            RichText::new("Nothing playing currently.")
                                                .font(FontId::monospace(10.0))
                                                .color(COLOR_TEXT_DIM),
                                        );
                                    } else {
                                        let cur_m = st.duration_ms / 60000;
                                        let cur_s = (st.duration_ms % 60000) / 1000;
                                        let dur_str = format!("{:02}:{:02}", cur_m, cur_s);

                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new("▶ ")
                                                    .font(FontId::monospace(10.0))
                                                    .color(COLOR_ACCENT_ROSE),
                                            );

                                            ui.vertical(|ui| {
                                                ui.label(
                                                    RichText::new(&st.title)
                                                        .font(FontId::monospace(11.0))
                                                        .strong()
                                                        .color(COLOR_ACCENT_ROSE),
                                                );
                                                let meta = format!("{} ({})", st.artist, dur_str);
                                                ui.label(
                                                    RichText::new(meta)
                                                        .font(FontId::monospace(9.5))
                                                        .color(COLOR_TEXT_DIM),
                                                );
                                            });
                                        });
                                    }

                                    ui.add_space(10.0);

                                    // Define render_queue_row closure before iterating items
                                    let render_queue_row = |ui: &mut egui::Ui,
                                                            num_label: String,
                                                            track: &SpotifyTrackItem,
                                                            cmd_tx: &mpsc::Sender<SoloistCommand>,
                                                            selected_track_uri: &mut Option<String>| {
                                        let dur_m = track.duration_ms / 60000;
                                        let dur_s = (track.duration_ms % 60000) / 1000;
                                        let dur_str = format!("{:02}:{:02}", dur_m, dur_s);

                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(num_label)
                                                    .font(FontId::monospace(10.0))
                                                    .color(COLOR_TEXT_DIM),
                                            );

                                            ui.vertical(|ui| {
                                                let label_resp = ui.selectable_label(
                                                    false,
                                                    RichText::new(&track.title)
                                                        .font(FontId::monospace(11.0))
                                                        .strong()
                                                        .color(COLOR_TEXT_BRIGHT),
                                                );

                                                if label_resp.clicked() {
                                                    *selected_track_uri = Some(track.uri.clone());
                                                    let _ = cmd_tx.blocking_send(SoloistCommand::PlayUri(track.uri.clone()));
                                                }

                                                let meta = format!("{} ({})", track.artist, dur_str);
                                                ui.label(
                                                    RichText::new(meta)
                                                        .font(FontId::monospace(9.5))
                                                        .color(COLOR_TEXT_DIM),
                                                );
                                            });
                                        });
                                        ui.add_space(4.0);
                                    };

                                    let manual_items = st.manual_queue_items.clone();
                                    let next_up_items = st.next_up_items.clone();
                                    let mut track_num = 1;

                                    // 2. NEXT IN QUEUE (Only renders if items were manually queued)
                                    if !manual_items.is_empty() {
                                        ui.label(
                                            RichText::new("Next in queue")
                                                .font(FontId::monospace(11.0))
                                                .strong()
                                                .color(COLOR_TEXT_BRIGHT),
                                        );
                                        ui.add_space(4.0);

                                        for track in &manual_items {
                                            let num_str = format!("{:02}.", track_num);
                                            render_queue_row(
                                                ui,
                                                num_str,
                                                track,
                                                &self.cmd_tx,
                                                &mut st.selected_track_uri,
                                            );
                                            track_num += 1;
                                        }

                                        ui.add_space(10.0);
                                    }

                                    // 3. NEXT UP (Context tracks only)
                                    ui.label(
                                        RichText::new("Next up")
                                            .font(FontId::monospace(11.0))
                                            .strong()
                                            .color(COLOR_TEXT_BRIGHT),
                                    );
                                    ui.add_space(4.0);

                                    if next_up_items.is_empty() && manual_items.is_empty() {
                                        ui.label(
                                            RichText::new("No upcoming tracks.")
                                                .font(FontId::monospace(10.0))
                                                .color(COLOR_TEXT_DIM),
                                        );
                                    } else {
                                        for track in &next_up_items {
                                            let num_str = format!("{:02}.", track_num);
                                            render_queue_row(
                                                ui,
                                                num_str,
                                                track,
                                                &self.cmd_tx,
                                                &mut st.selected_track_uri,
                                            );
                                            track_num += 1;
                                        }
                                    }
                                } else {
                                // --- NOW PLAYING & ARTIST VIEW ---
                                let art_size = (right_w - 20.0).max(120.0);

                                // 1. Album Cover Image
                                if let Some(ref url) = st.cover_url {
                                    ui.add(
                                        egui::Image::new(url)
                                            .fit_to_exact_size(Vec2::new(art_size, art_size))
                                            .rounding(Rounding::ZERO),
                                    );
                                } else {
                                    let (art_rect, _) = ui.allocate_exact_size(Vec2::new(art_size, art_size), egui::Sense::hover());
                                    ui.painter().rect_filled(art_rect, Rounding::ZERO, Color32::from_rgb(28, 29, 39));
                                    ui.painter().text(
                                        art_rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        "♪",
                                        FontId::monospace(24.0),
                                        COLOR_TEXT_DIM,
                                    );
                                }

                                ui.add_space(10.0);
                                // 2. Track Title & Inline Like Indicator
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 0.0);
                                    let max_title_w = (right_w - 56.0).max(40.0);

                                    let title_resp = Self::render_inline_link_tight(
                                        ui,
                                        &st.title,
                                        COLOR_TEXT_BRIGHT,
                                        FontId::monospace(13.0),
                                        max_title_w,
                                    );

                                    if title_resp.clicked() {
                                        if let Some(ref al_id) = st.current_album_id {
                                            let target = NavLocation::Album { id: al_id.clone(), title: st.album.clone() };
                                            st.navigate_to(target.clone());
                                            dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                        }
                                    }

                                    let active_uri = st.selected_track_uri.clone();
                                    let is_saved = active_uri.as_ref()
                                        .map_or(false, |u| st.liked_track_uris.contains(u));

                                    let (icon, col) = if is_saved {
                                        ("♥", COLOR_ACCENT_ROSE)
                                    } else {
                                        ("♡", COLOR_TEXT_DIM)
                                    };

                                    let (heart_rect, heart_resp) = ui.allocate_exact_size(Vec2::new(18.0, 16.0), egui::Sense::click());
                                    let heart_col = if heart_resp.hovered() { COLOR_TEXT_BRIGHT } else { col };

                                    ui.painter().text(
                                        heart_rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        icon,
                                        FontId::monospace(12.0),
                                        heart_col,
                                    );

                                    if heart_resp.clicked() {
                                        if let Some(ref uri) = active_uri {
                                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::ToggleLibraryUri {
                                                uri: uri.clone(),
                                                current_state: is_saved,
                                                is_context: false,
                                            });
                                        }
                                    }
                                });

                                ui.add_space(2.0);

                                // 3. Artist Name with Hover Effect
                                let artist_resp = Self::render_inline_link(
                                    ui,
                                    &st.artist,
                                    COLOR_TEXT_DIM,
                                    FontId::monospace(11.0),
                                    (right_w - 24.0).max(40.0),
                                );

                                if artist_resp.clicked() {
                                    if let Some(ref a_id) = st.current_artist_id {
                                        let target = NavLocation::Artist { id: a_id.clone() };
                                        st.navigate_to(target.clone());
                                        dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                    }
                                }

                                ui.add_space(10.0);
                                ui.separator();
                                ui.add_space(8.0);

                                // 4. About the Artist Section with Wrapping
                                ui.label(
                                    RichText::new("About the artist")
                                        .font(FontId::monospace(12.0))
                                        .strong()
                                        .color(COLOR_TEXT_BRIGHT),
                                );
                                ui.add_space(4.0);

                                ui.label(
                                    RichText::new(&st.artist)
                                        .font(FontId::monospace(11.0))
                                        .color(COLOR_ACCENT_GREEN),
                                );
                                ui.add_space(3.0);

                                // --- Followers & Monthly Listeners Stats ---
                                let followers = st.artist_followers
                                    .or_else(|| st.artist_page.as_ref().map(|p| p.followers));
                                let monthly = st.artist_monthly_listeners
                                    .or_else(|| st.artist_page.as_ref().map(|p| p.monthly_listeners));

                                if followers.is_some() || monthly.is_some() {
                                    let mut stats_parts = Vec::new();

                                    if let Some(f) = followers {
                                        let f_str = if f >= 1_000_000 {
                                            format!("{:.1}M", f as f64 / 1_000_000.0)
                                        } else if f >= 1_000 {
                                            format!("{:.0}K", f as f64 / 1_000.0)
                                        } else {
                                            format!("{f}")
                                        };
                                        stats_parts.push(format!("👥 {f_str} followers"));
                                    }

                                    if let Some(m) = monthly {
                                        let m_str = if m >= 1_000_000 {
                                            format!("{:.1}M", m as f64 / 1_000_000.0)
                                        } else if m >= 1_000 {
                                            format!("{:.0}K", m as f64 / 1_000.0)
                                        } else {
                                            format!("{m}")
                                        };
                                        stats_parts.push(format!("🎧 {m_str} monthly"));
                                    }

                                    ui.label(
                                        RichText::new(stats_parts.join("  •  "))
                                            .font(FontId::monospace(10.0))
                                            .color(COLOR_SUBTLE),
                                    );
                                    ui.add_space(6.0);
                                }

                                // --- Artist Biography ---
                                let bio_text = st.artist_bio.as_deref()
                                    .or_else(|| st.artist_page.as_ref().map(|p| p.bio.as_str()))
                                    .filter(|b| !b.trim().is_empty())
                                    .map(|b| b.to_string());

                                if let Some(bio) = bio_text {
                                    let segments = parse_spotify_html_bio(&bio);
                                    // Render directly into the outer ScrollArea without a nested ScrollArea
                                    self.render_bio_segments(ui, &mut *st, &segments);
                                }
                            }
                        });
                    });
                });

                // --- BOTTOM PLAYBACK CONTAINER ---
                ui.add_space(row_gap);

                let bottom_box_w = ui.available_width();
                // Consuming remaining available_height guarantees identical 8.0px window margins all around
                let bottom_box_h = ui.available_height();

                self.render_tui_box(ui, "Playing", false, Vec2::new(bottom_box_w, bottom_box_h), |ui| {
                    let avail_rect = ui.available_rect_before_wrap();
                    let center_x = avail_rect.center().x;

                    // --- Vertical Layout & Symmetrical Gaps ---
                    // Note: avail_rect.min.y is already 14.0px below the box top border.
                    // Gap from top border to Track Title = 14.0px.
                    let text_block_h = 28.0_f32; // Title (12pt) + 2px space + Artist (10pt)
                    let gap_to_scrubber = 14.0_f32; // Exactly matches the 14px top gap
                    let bar_h = 13.0_f32;

                    let text_top_y = avail_rect.min.y;
                    let bar_y = text_top_y + text_block_h + gap_to_scrubber;

                    let btn_size = Vec2::new(24.0, 24.0);
                    let btn_y = text_top_y + ((text_block_h - btn_size.y) / 2.0).round();

                    // 1. LEFT: Track Title, Inline Heart, Inline Plus, & Artist
                    let left_w = (center_x - avail_rect.min.x - 110.0).max(140.0);
                    let left_rect = Rect::from_min_size(Pos2::new(avail_rect.min.x, text_top_y), Vec2::new(left_w, text_block_h));
                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(left_rect), |ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing = Vec2::new(6.0, 0.0);
                                let max_title_w = (left_w - 48.0).max(40.0);

                                // Track Title (links to album)
                                let title_resp = Self::render_inline_link_tight(
                                    ui,
                                    &st.title,
                                    COLOR_TEXT_BRIGHT,
                                    FontId::monospace(12.0),
                                    max_title_w,
                                );
                                if title_resp.clicked() {
                                    if let Some(ref al_id) = st.current_album_id {
                                        let target = NavLocation::Album { id: al_id.clone(), title: st.album.clone() };
                                        st.navigate_to(target.clone());
                                        dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                    }
                                }

                                // Like Button inline with title
                                let active_uri = st.selected_track_uri.clone();
                                let is_saved = active_uri.as_ref()
                                    .map_or(false, |u| st.liked_track_uris.contains(u));

                                let (like_icon, like_col) = if is_saved {
                                    ("♥", COLOR_ACCENT_ROSE)
                                } else {
                                    ("♡", COLOR_TEXT_DIM)
                                };

                                let (heart_rect, heart_resp) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), egui::Sense::click());
                                let h_col = if heart_resp.hovered() { COLOR_TEXT_BRIGHT } else { like_col };
                                ui.painter().text(heart_rect.center(), egui::Align2::CENTER_CENTER, like_icon, FontId::monospace(11.5), h_col);

                                if heart_resp.clicked() {
                                    if let Some(ref uri) = active_uri {
                                        let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::ToggleLibraryUri {
                                            uri: uri.clone(),
                                            current_state: is_saved,
                                            is_context: false,
                                        });
                                    }
                                }

                                // Add-to-Queue Plus inline with title
                                let (plus_rect, plus_resp) = ui.allocate_exact_size(Vec2::new(14.0, 16.0), egui::Sense::click());
                                let p_col = if plus_resp.hovered() { COLOR_TEXT_BRIGHT } else { COLOR_TEXT_DIM };
                                ui.painter().text(plus_rect.center(), egui::Align2::CENTER_CENTER, "＋", FontId::monospace(11.0), p_col);

                                if plus_resp.on_hover_text("Add current track to queue").clicked() {
                                    if let Some(ref uri) = active_uri {
                                        let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::AddToQueue {
                                            track_uri: uri.clone(),
                                        });
                                    }
                                }
                            });

                            // Artists links
                            if !st.current_artists.is_empty() {
                                let mut bottom_nav: Option<NavLocation> = None;
                                Self::render_artist_links(
                                    ui,
                                    &st.current_artists,
                                    COLOR_TEXT_DIM,
                                    FontId::monospace(10.0),
                                    &mut bottom_nav,
                                );
                                if let Some(target) = bottom_nav {
                                    st.navigate_to(target.clone());
                                    dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                }
                            } else {
                                let single_artist = vec![(st.artist.clone(), st.current_artist_id.clone().unwrap_or_default())];
                                let mut bottom_nav: Option<NavLocation> = None;
                                Self::render_artist_links(
                                    ui,
                                    &single_artist,
                                    COLOR_TEXT_DIM,
                                    FontId::monospace(10.0),
                                    &mut bottom_nav,
                                );
                                if let Some(target) = bottom_nav {
                                    st.navigate_to(target.clone());
                                    dispatch_nav(target, &mut st, &self.spotify_req_tx);
                                }
                            }
                        });
                    });

                    // 2. CENTER: Transport Controls
                    let gap = 8.0;
                    let half_play = btn_size.x / 2.0;

                    let play_x = center_x - half_play;
                    let prev_x = play_x - gap - btn_size.x;
                    let shuf_x = prev_x - gap - btn_size.x;
                    let next_x = play_x + btn_size.x + gap;
                    let rep_x  = next_x + btn_size.x + gap;

                    // Shuffle
                    let s_col = if st.shuffle_state { COLOR_ACCENT_ROSE } else { COLOR_TEXT_DIM };
                    if ui.put(
                        Rect::from_min_size(Pos2::new(shuf_x, btn_y), btn_size),
                        egui::Button::new(RichText::new("🔀").size(11.0).color(s_col)).fill(Color32::TRANSPARENT),
                    ).clicked() {
                        st.shuffle_state = !st.shuffle_state;
                        let _ = self.cmd_tx.blocking_send(SoloistCommand::ToggleShuffle);
                    }

                    // Prev
                    if ui.put(
                        Rect::from_min_size(Pos2::new(prev_x, btn_y), btn_size),
                        egui::Button::new(RichText::new("⏮").size(11.0).color(COLOR_TEXT_DIM)).fill(Color32::TRANSPARENT),
                    ).clicked() {
                        let _ = self.cmd_tx.blocking_send(SoloistCommand::Previous);
                    }

                    // Play / Pause
                    let p_icon = if st.is_playing { "⏸" } else { "▶" };
                    if ui.put(
                        Rect::from_min_size(Pos2::new(play_x, btn_y), btn_size),
                        egui::Button::new(RichText::new(p_icon).size(11.0).color(COLOR_TEXT_BRIGHT)).fill(Color32::TRANSPARENT),
                    ).clicked() {
                        let _ = self.cmd_tx.blocking_send(SoloistCommand::Toggle);
                    }

                    // Next
                    if ui.put(
                        Rect::from_min_size(Pos2::new(next_x, btn_y), btn_size),
                        egui::Button::new(RichText::new("⏭").size(11.0).color(COLOR_TEXT_DIM)).fill(Color32::TRANSPARENT),
                    ).clicked() {
                        let _ = self.cmd_tx.blocking_send(SoloistCommand::Next);
                    }

                    // Repeat
                    let (r_icon, r_col) = match st.repeat_state {
                        1 => ("🔁", COLOR_ACCENT_ROSE),
                        2 => ("🔂", COLOR_ACCENT_ROSE),
                        _ => ("🔁", COLOR_TEXT_DIM),
                    };
                    if ui.put(
                        Rect::from_min_size(Pos2::new(rep_x, btn_y), btn_size),
                        egui::Button::new(RichText::new(r_icon).size(11.0).color(r_col)).fill(Color32::TRANSPARENT),
                    ).clicked() {
                        st.repeat_state = (st.repeat_state + 1) % 3;
                        let _ = self.cmd_tx.blocking_send(SoloistCommand::CycleRepeat);
                    }

                    // 3. RIGHT: Device Picker, Queue Button & Dashed Volume
                    let num_dashes = 10;
                    let dash_w = 6.0_f32;
                    let dash_h = 2.0_f32;
                    let dash_gap = 4.0_f32;
                    let total_dashes_w = (num_dashes as f32 * dash_w) + ((num_dashes - 1) as f32 * dash_gap);

                    let right_margin = 10.0_f32;
                    let dashes_end_x = avail_rect.max.x - right_margin;
                    let dashes_start_x = dashes_end_x - total_dashes_w;
                    let center_y = btn_y + (btn_size.y / 2.0); // Exact vertical center of transport buttons

                    let icon_center_x = dashes_start_x - 14.0;
                    let queue_btn_x = (icon_center_x - 12.0) - gap - btn_size.x;
                    let device_btn_x = queue_btn_x - gap - btn_size.x;

                    // Device Connect Button
                    let device_btn_rect = Rect::from_min_size(Pos2::new(device_btn_x, btn_y), btn_size);
                    let device_resp = ui.interact(
                        device_btn_rect,
                        ui.id().with("bottom_bar_device_toggle"),
                        egui::Sense::click(),
                    );

                    let d_col = if st.show_device_menu {
                        COLOR_ACCENT_ROSE
                    } else if device_resp.hovered() {
                        COLOR_TEXT_BRIGHT
                    } else {
                        COLOR_TEXT_DIM
                    };

                    draw_crisp_device_icon(ui.painter(), device_btn_rect.center(), d_col);

                    if device_resp.on_hover_text("Connect to a device").clicked() {
                        st.show_device_menu = !st.show_device_menu;
                        if st.show_device_menu {
                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::FetchDevices);
                        }
                    }

                    // Device Selector Flyout
                    if st.show_device_menu {
                        let mut close_device_menu = false;
                        let mut target_device: Option<String> = None;

                        // Position popup cleanly above the button
                        let popup_w = 210.0_f32;
                        let popup_h = 130.0_f32;
                        let popup_pos = Pos2::new(device_btn_x - (popup_w / 2.0) + (btn_size.x / 2.0), btn_y - popup_h - 8.0);

                        egui::Area::new(egui::Id::new("device_picker_popup_area"))
                            .fixed_pos(popup_pos)
                            .order(egui::Order::Tooltip)
                            .show(ctx, |ui| {
                                // Close when clicking anywhere outside both the popup and the toggle button
                                if ui.input(|i| i.pointer.primary_clicked()) {
                                    if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                                        if !ui.min_rect().contains(pos) && !device_btn_rect.contains(pos) {
                                            close_device_menu = true;
                                        }
                                    }
                                }

                                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    close_device_menu = true;
                                }

                                let frame = egui::Frame::none()
                                    .fill(COLOR_SURFACE)
                                    .stroke(Stroke::new(1.0_f32, COLOR_BORDER))
                                    .inner_margin(egui::Margin::symmetric(8.0, 8.0));

                                frame.show(ui, |ui| {
                                    ui.set_width(popup_w);
                                    ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new("CONNECT TO A DEVICE")
                                                    .font(FontId::monospace(10.0))
                                                    .color(COLOR_TEXT_DIM),
                                            );
                                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                                if ui.add(egui::Button::new(RichText::new("⟳").size(11.0).color(COLOR_TEXT_DIM)).fill(Color32::TRANSPARENT).frame(false)).on_hover_text("Refresh devices").clicked() {
                                                    let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::FetchDevices);
                                                }
                                            });
                                        });

                                        ui.add_space(4.0);
                                        ui.separator();
                                        ui.add_space(4.0);

                                        if st.available_devices.is_empty() {
                                            ui.label(
                                                RichText::new("No active devices found")
                                                    .font(FontId::monospace(10.5))
                                                    .color(COLOR_MUTED),
                                            );
                                        } else {
                                            for dev in &st.available_devices {
                                                let is_active = dev.is_active;
                                                let dev_col = if is_active { COLOR_ACCENT_ROSE } else { COLOR_TEXT_BRIGHT };
                                                let icon = match dev.device_type.as_str() {
                                                    "Smartphone" => "📱",
                                                    "Computer" => "💻",
                                                    "Speaker" => "🔈",
                                                    _ => "🔊",
                                                };

                                                let label = format!("{} {}", icon, dev.name);
                                                let dev_btn = ui.add(
                                                    egui::Button::new(RichText::new(label).font(FontId::monospace(11.0)).color(dev_col))
                                                        .fill(if is_active { Color32::from_white_alpha(12) } else { Color32::TRANSPARENT })
                                                        .frame(true)
                                                );

                                                if dev_btn.clicked() {
                                                    target_device = Some(dev.id.clone());
                                                    close_device_menu = true;
                                                }
                                            }
                                        }
                                    });
                                });
                            });

                        if close_device_menu {
                            st.show_device_menu = false;
                        }

                        if let Some(dev_id) = target_device {
                            let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::TransferPlayback {
                                device_id: dev_id,
                                play: true,
                            });
                        }
                    }

                    // --- Queue Drawer Button ---
                    let queue_btn_rect = Rect::from_min_size(Pos2::new(queue_btn_x, btn_y), btn_size);
                    let queue_resp = ui.interact(queue_btn_rect, ui.id().with("bottom_bar_queue_toggle"), egui::Sense::click());
                    let q_col = if st.show_queue {
                        COLOR_ACCENT_ROSE
                    } else if queue_resp.hovered() {
                        COLOR_TEXT_BRIGHT
                    } else {
                        COLOR_TEXT_DIM
                    };

                    ui.painter().text(
                        queue_btn_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "☰",
                        FontId::monospace(12.0),
                        q_col,
                    );

                    if queue_resp.on_hover_text("Upcoming Queue").clicked() {
                        st.show_queue = !st.show_queue;
                    }

                    // --- Volume Area (Crisp Speaker Icon + Dashes) ---
                    let vol_area_rect = Rect::from_min_max(
                        Pos2::new(icon_center_x - 8.0, btn_y),
                        Pos2::new(dashes_end_x + 2.0, btn_y + btn_size.y),
                    );
                    let vol_resp = ui.interact(vol_area_rect, ui.id().with("playing_bar_volume_slider"), egui::Sense::click_and_drag());

                    let scroll_delta = ui.input(|i| i.raw_scroll_delta.y);
                    if vol_resp.hovered() && scroll_delta.abs() > 0.0 {
                        let step = if scroll_delta > 0.0 { 10i16 } else { -10i16 };
                        let new_vol = (st.volume as i16 + step).clamp(0, 100) as u8;
                        if new_vol != st.volume {
                            st.volume = new_vol;
                            let _ = self.cmd_tx.blocking_send(SoloistCommand::SetVolume(new_vol));
                        }
                    }

                    if (vol_resp.clicked() || vol_resp.dragged()) && vol_resp.interact_pointer_pos().is_some() {
                        let mouse_x = vol_resp.interact_pointer_pos().unwrap().x;
                        let frac = ((mouse_x - dashes_start_x) / total_dashes_w).clamp(0.0, 1.0);
                        let target_vol = (frac * 100.0).round() as u8;
                        if target_vol != st.volume {
                            st.volume = target_vol;
                            let _ = self.cmd_tx.blocking_send(SoloistCommand::SetVolume(target_vol));
                        }
                    }

                    let is_vol_active = vol_resp.hovered() || vol_resp.dragged();
                    let speaker_col = if is_vol_active {
                        COLOR_TEXT_BRIGHT
                    } else if st.volume == 0 {
                        COLOR_MUTED
                    } else {
                        COLOR_TEXT_DIM
                    };

                    draw_crisp_speaker_icon(ui.painter(), Pos2::new(icon_center_x, center_y), speaker_col, st.volume == 0);

                    // Dashes share the exact same center_y
                    let active_count = ((st.volume as f32 / 100.0) * num_dashes as f32).round() as usize;
                    for i in 0..num_dashes {
                        let dash_x = dashes_start_x + (i as f32 * (dash_w + dash_gap));
                        let dash_rect = Rect::from_center_size(
                            Pos2::new(dash_x + (dash_w / 2.0), center_y),
                            Vec2::new(dash_w, dash_h),
                        );

                        let color = if i < active_count {
                            COLOR_ACCENT_ROSE
                        } else {
                            COLOR_OVERLAY
                        };

                        ui.painter().rect_filled(dash_rect, Rounding::ZERO, color);
                    }

                    // 4. BOTTOM: Scrubber Bar (Has 13px clearance to the bottom border)
                    let bar_rect = Rect::from_min_size(Pos2::new(avail_rect.min.x, bar_y), Vec2::new(avail_rect.width(), bar_h));
                    let bar_resp = ui.interact(bar_rect, ui.id().with("playing_bar_scrubber_track"), egui::Sense::click_and_drag());

                    let effective_duration = if st.duration_ms > 0 { st.duration_ms } else { 180_000 };

                    if bar_resp.drag_started() {
                        st.is_dragging_scrubber = true;
                    }
                    if bar_resp.clicked() || bar_resp.dragged() {
                        if let Some(mouse_pos) = bar_resp.interact_pointer_pos() {
                            let frac = ((mouse_pos.x - bar_rect.min.x) / bar_rect.width()).clamp(0.0, 1.0);
                            st.scrubber_drag_val = (frac * effective_duration as f32) as f64;
                        }
                    }
                    if bar_resp.drag_stopped() || bar_resp.clicked() {
                        st.is_dragging_scrubber = false;
                        let seek_target = (st.scrubber_drag_val.round() as u64).min(effective_duration);
                        st.position_ms = seek_target;
                        st.last_sync = Instant::now();
                        let _ = self.cmd_tx.blocking_send(SoloistCommand::Seek(seek_target));
                    }

                    let elapsed = if st.is_playing {
                        st.last_sync.elapsed().as_millis() as u64
                    } else {
                        0
                    };
                    let current_pos_ms = (st.position_ms + elapsed).min(st.duration_ms.max(1));

                    let progress_frac = if st.duration_ms > 0 {
                        (current_pos_ms as f32 / st.duration_ms as f32).clamp(0.0, 1.0)
                    } else {
                        (current_pos_ms as f32 / 180_000.0).clamp(0.0, 1.0)
                    };

                    let cur_m = current_pos_ms / 60_000;
                    let cur_s = (current_pos_ms % 60_000) / 1_000;
                    let dur_m = st.duration_ms / 60_000;
                    let dur_s = (st.duration_ms % 60_000) / 1_000;

                    let time_str = if st.duration_ms > 0 {
                        format!("{}:{:02} / {}:{:02}", cur_m, cur_s, dur_m, dur_s)
                    } else {
                        format!("{}:{:02} / --:--", cur_m, cur_s)
                    };

                    let fill_width = bar_rect.width() * progress_frac;

                    // Draw track groove
                    ui.painter().rect_filled(bar_rect, Rounding::ZERO, Color32::from_rgb(32, 34, 44));

                    // Draw progress bar fill
                    let filled_bound = Rect::from_min_max(bar_rect.min, Pos2::new(bar_rect.min.x + fill_width, bar_rect.max.y));
                    if fill_width > 0.0 {
                        let filled_bound = Rect::from_min_max(bar_rect.min, Pos2::new(bar_rect.min.x + fill_width, bar_rect.max.y));
                        ui.painter().rect_filled(filled_bound, Rounding::ZERO, COLOR_ACCENT_ROSE);
                    }

                    // Inverted centered text overlay
                    let font = FontId::monospace(10.0);
                    let text_center = Pos2::new(center_x, bar_rect.center().y);
                    let unfilled_bound = Rect::from_min_max(Pos2::new(bar_rect.min.x + fill_width, bar_rect.min.y), bar_rect.max);

                    let mut green_painter = ui.painter().clone();
                    green_painter.set_clip_rect(filled_bound.intersect(bar_rect));
                    green_painter.text(text_center, egui::Align2::CENTER_CENTER, &time_str, font.clone(), COLOR_BG);

                    let mut dark_painter = ui.painter().clone();
                    dark_painter.set_clip_rect(unfilled_bound.intersect(bar_rect));
                    dark_painter.text(text_center, egui::Align2::CENTER_CENTER, &time_str, font, COLOR_TEXT_BRIGHT);
                });

                // --- Track Right-Click Context Menu (Single Root Instance) ---
                let mut close_menu = false;
                let mut menu_req: Option<SpotifyRequest> = None;

                if let Some(mut menu) = st.context_menu.take() {
                    let menu_pos = menu.click_pos;
                    let track = menu.track.clone();
                    let user_playlists = st.user_playlists.clone();

                    egui::Area::new(egui::Id::new("track_context_menu_area"))
                        .fixed_pos(menu_pos)
                        .order(egui::Order::Tooltip)
                        .show(ctx, |ui| {
                            // Dismiss when clicking outside the menu area
                            if ui.input(|i| i.pointer.primary_clicked()) {
                                if let Some(click_pos) = ui.input(|i| i.pointer.interact_pos()) {
                                    if !ui.min_rect().contains(click_pos) {
                                        close_menu = true;
                                    }
                                }
                            }

                            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                close_menu = true;
                            }

                            // Helper to render consistent menu buttons with full hover borders and text truncation
                            let render_item = |ui: &mut egui::Ui, label: &str, is_active: bool, has_arrow: bool| -> egui::Response {
                                let w = ui.available_width();
                                let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 24.0_f32), egui::Sense::click());

                                let is_hovered = resp.hovered() || is_active;
                                if is_hovered {
                                    ui.painter().rect_filled(rect, Rounding::ZERO, Color32::from_white_alpha(14));
                                    ui.painter().rect_stroke(rect, Rounding::ZERO, ui.visuals().widgets.hovered.bg_stroke);
                                }

                                let font = FontId::monospace(11.0);
                                let text_col = if is_hovered { COLOR_TEXT_BRIGHT } else { COLOR_SUBTLE };

                                let right_reserved = if has_arrow { 22.0_f32 } else { 8.0_f32 };
                                let text_max_w = (rect.width() - 10.0 - right_reserved).max(10.0);

                                let galley = ui.painter().layout(
                                    label.to_string(),
                                    font.clone(),
                                    text_col,
                                    text_max_w,
                                );

                                let text_y = rect.center().y - (galley.size().y / 2.0);
                                ui.painter().galley(Pos2::new(rect.min.x + 8.0, text_y), galley, text_col);

                                if has_arrow {
                                    ui.painter().text(
                                        Pos2::new(rect.max.x - 8.0, rect.center().y),
                                        egui::Align2::RIGHT_CENTER,
                                        "▶",
                                        font,
                                        text_col,
                                    );
                                }

                                resp
                            };

                            ui.horizontal(|ui| {
                                // Add visible gap between primary menu and the flyout submenu
                                ui.spacing_mut().item_spacing.x = 4.0;

                                // 1. Primary Context Menu Column
                                let menu_frame = egui::Frame::none()
                                    .fill(COLOR_SURFACE)
                                    .stroke(Stroke::new(1.0_f32, COLOR_BORDER))
                                    .inner_margin(egui::Margin::symmetric(4.0, 4.0));

                                menu_frame.show(ui, |ui| {
                                    ui.vertical(|ui| {
                                        ui.set_width(205.0);
                                        ui.spacing_mut().item_spacing.y = 2.0;

                                        let in_liked_songs = st.active_nav == "Liked Songs";
                                        let active_context_uri = st.active_context_uri.clone();

                                        // Add to Playlist (hover or click unfolds submenu)
                                        let add_pl_resp = render_item(ui, "+  Add to playlist", menu.show_playlist_submenu, true);
                                        if add_pl_resp.hovered() || add_pl_resp.clicked() {
                                            menu.show_playlist_submenu = true;
                                        }

                                        // Check ownership: user can only remove tracks if they own the active playlist
                                        let is_playlist_owner = st.is_context_owner
                                            || st.user_playlists.iter().any(|pl| {
                                            if let Some(ref uri) = st.active_context_uri {
                                                let clean_pl = uri.trim_start_matches("spotify:playlist:");
                                                pl.id == clean_pl && st.current_user_id.as_deref() == Some(&pl.owner_id)
                                            } else {
                                                false
                                            }
                                        });

                                        let in_playlist = st.active_context_uri.as_ref().map_or(false, |u| u.starts_with("spotify:playlist:"));

                                        // Render Remove from playlist only if inside an owned playlist
                                        if in_playlist && is_playlist_owner {
                                            let remove_resp = render_item(ui, "⊖  Remove from playlist", false, false);
                                            // Close flyout when hovering other items
                                            if remove_resp.hovered() {
                                                menu.show_playlist_submenu = false;
                                            }
                                            if remove_resp.clicked() {
                                                if let Some(ref uri) = active_context_uri {
                                                    let pos = menu.track_index;

                                                    if pos < st.displayed_tracks.len() {
                                                        st.displayed_tracks.remove(pos);
                                                    }

                                                    let remaining_uris: Vec<String> = st.displayed_tracks
                                                        .iter()
                                                        .map(|t| t.uri.clone())
                                                        .collect();

                                                    menu_req = Some(SpotifyRequest::RemoveFromPlaylist {
                                                        playlist_id: uri.clone(),
                                                        updated_uris: remaining_uris,
                                                    });

                                                    close_menu = true;
                                                }
                                            }
                                        }

                                        // Save / Remove from Liked Songs
                                        let (like_label, next_state) = if menu.is_saved {
                                            ("♥  Remove from Liked Songs", false)
                                        } else {
                                            ("♡  Save to Liked Songs", true)
                                        };

                                        let save_resp = render_item(ui, like_label, false, false);
                                        // Close flyout when hovering other items
                                        if save_resp.hovered() {
                                            menu.show_playlist_submenu = false;
                                        }
                                        if save_resp.clicked() {
                                            menu_req = Some(SpotifyRequest::ToggleLibraryUri {
                                                uri: track.uri.clone(),
                                                current_state: menu.is_saved,
                                                is_context: false,
                                            });

                                            if in_liked_songs && !next_state {
                                                st.displayed_tracks.retain(|t| t.uri != track.uri);
                                            }

                                            close_menu = true;
                                        }

                                        // Add to Queue
                                        let queue_resp = render_item(ui, "☰  Add to queue", false, false);
                                        // Close flyout when hovering other items
                                        if queue_resp.hovered() {
                                            menu.show_playlist_submenu = false;
                                        }
                                        if queue_resp.clicked() {
                                            menu_req = Some(SpotifyRequest::AddToQueue {
                                                track_uri: track.uri.clone(),
                                            });
                                            close_menu = true;
                                        }
                                    });
                                });

                                // 2. Flyout Submenu: Playlist List directly without search bar
                                if menu.show_playlist_submenu {
                                    let sub_frame = egui::Frame::none()
                                        .fill(COLOR_SURFACE)
                                        .stroke(Stroke::new(1.0_f32, COLOR_BORDER))
                                        .inner_margin(egui::Margin::symmetric(4.0, 4.0));

                                    sub_frame.show(ui, |ui| {
                                        ui.set_clip_rect(ui.max_rect());
                                        ui.vertical(|ui| {
                                            ui.set_width(215.0);
                                            ui.spacing_mut().item_spacing.y = 2.0;

                                            egui::ScrollArea::vertical()
                                                .id_salt("single_context_playlist_sublist")
                                                .max_height(220.0)
                                                .show(ui, |ui| {
                                                    ui.vertical(|ui| {
                                                        ui.set_width(205.0);
                                                        ui.spacing_mut().item_spacing.y = 2.0;

                                                        for pl in &user_playlists {
                                                            let pl_label = format!("📁 {}", pl.name);
                                                            let pl_resp = render_item(ui, &pl_label, false, false);

                                                            if pl_resp.clicked() {
                                                                menu_req = Some(SpotifyRequest::AddToPlaylist {
                                                                    playlist_id: pl.id.clone(),
                                                                    track_uri: track.uri.clone(),
                                                                });
                                                                close_menu = true;
                                                            }
                                                        }
                                                    });
                                                });
                                        });
                                    });
                                }
                            });
                        });

                    if !close_menu {
                        st.context_menu = Some(menu);
                    }
                }

                if let Some(req) = menu_req {
                    let _ = self.spotify_req_tx.blocking_send(req);
                }
            });


        // First-run client ID dialog
        if st.show_key_prompt {
            egui::Area::new(egui::Id::new("key_prompt_modal_area"))
                .fixed_pos(Pos2::new(0.0, 0.0))
                .show(ctx, |ui| {
                    let screen_rect = ctx.screen_rect();
                    ui.painter().rect_filled(
                        screen_rect,
                        Rounding::ZERO,
                        Color32::from_black_alpha(180),
                    );

                    let modal_w = 480.0;
                    let modal_h = 240.0;
                    let center_pos = Pos2::new(
                        (screen_rect.width() - modal_w) / 2.0,
                        (screen_rect.height() - modal_h) / 2.0,
                    );

                    let modal_rect = Rect::from_min_size(center_pos, Vec2::new(modal_w, modal_h));
                    ui.painter().rect(
                        modal_rect,
                        Rounding::same(8.0),
                        COLOR_SURFACE,
                        Stroke::new(1.5_f32, COLOR_ACCENT_ROSE),
                    );

                    let mut child = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(modal_rect.shrink(18.0))
                            .layout(egui::Layout::top_down(egui::Align::Center)),
                    );

                    child.label(
                        RichText::new("SPOTIFY SETUP: CLIENT ID REQUIRED")
                            .font(FontId::monospace(14.0))
                            .strong()
                            .color(COLOR_ACCENT_ROSE),
                    );
                    child.add_space(8.0);
                    child.label(
                        RichText::new("To enable Catalog Search & Playlists in this client:\n1. Create a free app at developer.spotify.com/dashboard\n2. Add Redirect URI: http://127.0.0.1:8888/callback\n3. Paste your Client ID below:")
                            .font(FontId::monospace(11.0))
                            .color(COLOR_TEXT_BRIGHT),
                    );
                    child.add_space(10.0);

                    let edit = child.add(
                        egui::TextEdit::singleline(&mut st.input_client_id)
                            .hint_text("Paste Spotify Client ID here...")
                            .desired_width(400.0)
                            .font(FontId::monospace(12.0)),
                    );

                    child.add_space(12.0);

                    let can_submit = !st.input_client_id.trim().is_empty();
                    let submit_btn = egui::Button::new(
                        RichText::new("Save & Authenticate")
                            .font(FontId::monospace(12.0))
                            .color(Color32::BLACK)
                            .strong(),
                    )
                        .fill(if can_submit { COLOR_ACCENT_ROSE } else { Color32::DARK_GRAY })
                        .rounding(Rounding::same(4.0));

                    if (child.add(submit_btn).clicked()
                        || (edit.lost_focus() && child.input(|i| i.key_pressed(egui::Key::Enter))))
                        && can_submit
                    {
                        let id = st.input_client_id.trim().to_string();
                        let cfg = AppConfig { client_id: id.clone() };
                        let _ = cfg.save();

                        st.show_key_prompt = false;
                        let _ = self.spotify_req_tx.blocking_send(SpotifyRequest::Init(id));
                    }
                });
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // 1. Initialize and spawn the QEMU lossless guest backend
    let qemu_config = QemuConfig::default();
    let mut qemu_backend = QemuBackend::new(qemu_config);

    if let Err(e) = qemu_backend.start() {
        log::warn!("Could not spawn internal QEMU backend: {:?}. Proceeding with assumption daemon is running externally.", e);
    } else {
        log::info!("Headless QEMU guest backend successfully spawned.");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let running = Arc::new(AtomicBool::new(true));
    let audio_connected = Arc::new(AtomicBool::new(false));
    let ws_connected = Arc::new(AtomicBool::new(false));

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<SoloistCommand>(32);
    let (spotify_req_tx, mut spotify_req_rx) = mpsc::channel::<SpotifyRequest>(16);
    let (spotify_resp_tx, spotify_resp_rx) = mpsc::channel::<SpotifyResponse>(32);

    let config = AppConfig::load();
    let initial_prompt = config.client_id.trim().is_empty();

    let state = Arc::new(std::sync::RwLock::new(AppState {
        show_key_prompt: initial_prompt,
        ..Default::default()
    }));

    // 1. SMTC Service
    let smtc_cmd_tx = cmd_tx.clone();
    let (smtc_tx, smtc_rx) = std::sync::mpsc::channel::<AppState>();
    thread::spawn(move || {
        let _ = run_smtc_service(smtc_cmd_tx, smtc_rx);
    });

    // 2. WASAPI Renderer
    let (mut producer, consumer) = RingBuffer::<u8>::new(RING_BUFFER_CAPACITY_BYTES);
    let _audio_thread = thread::Builder::new()
        .name("wasapi-render".into())
        .spawn(move || {
            let _ = run_wasapi_shared_renderer(consumer);
        })?;

    // 3. Audio Ingestion Task (TCP 4714)
    let running_tcp = running.clone();
    let audio_conn_flag = audio_connected.clone();
    rt.spawn(async move {
        let stream_addr: SocketAddr = "127.0.0.1:4714".parse().unwrap();
        while running_tcp.load(Ordering::Relaxed) {
            let mut stream = match TcpStream::connect(stream_addr).await {
                Ok(s) => {
                    audio_conn_flag.store(true, Ordering::Relaxed);
                    s
                }
                Err(_) => {
                    audio_conn_flag.store(false, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    continue;
                }
            };

            let mut chunk = vec![0u8; 8192];
            loop {
                if !running_tcp.load(Ordering::Relaxed) {
                    break;
                }

                match stream.read(&mut chunk).await {
                    Ok(0) => {
                        audio_conn_flag.store(false, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                        break;
                    }
                    Ok(bytes_read) => {
                        audio_conn_flag.store(true, Ordering::Relaxed);
                        let mut written = 0;
                        while written < bytes_read {
                            match producer.write_chunk(bytes_read - written) {
                                Ok(mut chunk_slice) => {
                                    let (first, second) = chunk_slice.as_mut_slices();
                                    let flen = first.len();
                                    let slen = second.len();
                                    first.copy_from_slice(&chunk[written..written + flen]);
                                    if slen > 0 {
                                        second.copy_from_slice(&chunk[written + flen..written + flen + slen]);
                                    }
                                    let total_read = flen + slen;
                                    chunk_slice.commit_all();
                                    written += total_read;
                                }
                                Err(_) => {
                                    tokio::time::sleep(Duration::from_millis(5)).await;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        audio_conn_flag.store(false, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                        break;
                    }
                }
            }
        }
    });

    // 4. WebSocket Client Task (Port 3000)
    let running_ws = running.clone();
    let ws_conn_flag = ws_connected.clone();
    let state_ws = state.clone();
    let smtc_updater = smtc_tx.clone();

    rt.spawn(async move {
        let ws_url = "ws://127.0.0.1:3000";
        while running_ws.load(Ordering::Relaxed) {
            match connect_async(ws_url).await {
                Ok((ws_stream, _)) => {
                    ws_conn_flag.store(true, Ordering::Relaxed);
                    let (mut write, mut read) = ws_stream.split();

                    // 1. Query initial state first so we have the current context and position
                    let get_state_payload = serde_json::json!({
                        "type": "command",
                        "command": "get_state"
                    });
                    log::info!("--> Querying initial playback state...");
                    let _ = write.send(Message::Text(get_state_payload.to_string())).await;

                    tokio::time::sleep(Duration::from_millis(150)).await;

                    // 2. Set playback volume to 100%
                    let vol_payload = serde_json::json!({
                        "type": "command",
                        "command": "set_volume",
                        "volume": 100
                    });
                    log::info!("--> Setting initial playback volume to 100%...");
                    let _ = write.send(Message::Text(vol_payload.to_string())).await;

                    if let Ok(mut st) = state_ws.write() {
                        st.volume = 100;
                    }

                    tokio::time::sleep(Duration::from_millis(150)).await;

                    // 3. Activate session preserving context (do not pass empty track/index)
                    let activate_payload = serde_json::json!({
                        "type": "command",
                        "command": "activate"
                    });
                    log::info!("--> Activating Soloist session...");
                    let _ = write.send(Message::Text(activate_payload.to_string())).await;

                    tokio::time::sleep(Duration::from_millis(150)).await;

                    // 4. Resume playback cleanly (uses "resume" if supported by daemon, or bare "play" without uri)
                    let resume_payload = serde_json::json!({
                        "type": "command",
                        "command": "play"
                    });
                    log::info!("--> Resuming playback at existing position...");
                    let _ = write.send(Message::Text(resume_payload.to_string())).await;

                    tokio::time::sleep(Duration::from_millis(150)).await;

                    // 5. Fetch initial queue state
                    // 1. Startup handshake: request full upcoming queue
                    let get_queue_payload = serde_json::json!({
                        "type": "command",
                        "command": "get_queue",
                        "limit": 100 // Request up to 100 queued items instead of defaulting to 10
                    });
                    log::info!("--> Querying initial queue state (expanded)...");
                    let _ = write.send(Message::Text(get_queue_payload.to_string())).await;
                    loop {
                        tokio::select! {
                            Some(cmd) = cmd_rx.recv() => {
                                let payloads: Vec<serde_json::Value> = match cmd {
                                    SoloistCommand::Play => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "play"
                                    })],
                                    SoloistCommand::PlayUri(uri) => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "play",
                                        "uri": uri
                                    })],
                                    SoloistCommand::PlayContext { uri, .. } => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "play",
                                        "uri": uri
                                    })],
                                    SoloistCommand::Pause => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "pause"
                                    })],
                                    SoloistCommand::Toggle => {
                                        let is_playing = state_ws.read().map(|s| s.is_playing).unwrap_or(false);
                                        let c = if is_playing { "pause" } else { "play" };
                                        vec![serde_json::json!({
                                            "type": "command",
                                            "command": c
                                        })]
                                    }
                                    SoloistCommand::Next => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "skip_next"
                                    })],
                                    SoloistCommand::Previous => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "skip_prev"
                                    })],
                                    SoloistCommand::Seek(pos) => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "seek",
                                        "position_ms": pos
                                    })],
                                    SoloistCommand::Activate => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "activate"
                                    })],
                                    SoloistCommand::GetState => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "get_state"
                                    })],
                                    SoloistCommand::GetQueue => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "get_queue",
                                        "limit": 100
                                    })],
                                    SoloistCommand::SetVolume(vol) => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "set_volume",
                                        "volume": vol
                                    })],
                                    SoloistCommand::ToggleShuffle => {
                                        let current_shuffle = state_ws.read().map(|s| s.shuffle_state).unwrap_or(false);
                                        let target_shuffle = !current_shuffle;
                                        vec![serde_json::json!({
                                            "type": "command",
                                            "command": "set_shuffle",
                                            "enabled": target_shuffle
                                        })]
                                    }
                                    SoloistCommand::SetShuffle(enabled) => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "set_shuffle",
                                        "enabled": enabled
                                    })],
                                    SoloistCommand::CycleRepeat => {
                                        let current = state_ws.read().map(|s| s.repeat_state).unwrap_or(0);
                                        match current {
                                            0 => vec![
                                                serde_json::json!({
                                                    "type": "command",
                                                    "command": "set_repeat_track",
                                                    "enabled": false
                                                }),
                                                serde_json::json!({
                                                    "type": "command",
                                                    "command": "set_repeat_context",
                                                    "enabled": true
                                                }),
                                            ],
                                            1 => vec![
                                                serde_json::json!({
                                                    "type": "command",
                                                    "command": "set_repeat_context",
                                                    "enabled": false
                                                }),
                                                serde_json::json!({
                                                    "type": "command",
                                                    "command": "set_repeat_track",
                                                    "enabled": true
                                                }),
                                            ],
                                            _ => vec![
                                                serde_json::json!({
                                                    "type": "command",
                                                    "command": "set_repeat_track",
                                                    "enabled": false
                                                }),
                                                serde_json::json!({
                                                    "type": "command",
                                                    "command": "set_repeat_context",
                                                    "enabled": false
                                                }),
                                            ],
                                        }
                                    }
                                    SoloistCommand::SetRepeatContext(enabled) => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "set_repeat_context",
                                        "enabled": enabled
                                    })],
                                    SoloistCommand::SetRepeatTrack(enabled) => vec![serde_json::json!({
                                        "type": "command",
                                        "command": "set_repeat_track",
                                        "enabled": enabled
                                    })],
                                };

                                for p in payloads {
                                    let msg = p.to_string();
                                    log::info!("--> WebSocket Outbound: {}", msg);
                                    if let Err(e) = write.send(Message::Text(msg)).await {
                                        log::error!("Failed to send command to Soloist: {:?}", e);
                                        break;
                                    }
                                }
                            }

                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        log::info!("<-- Inbound WS Frame: {}", text);
                                        if let Ok(val) = serde_json::from_str::<Value>(&text) {
                                            if let Ok(mut st) = state_ws.write() {
                                                handle_soloist_event(&val, &mut st);
                                                let _ = smtc_updater.send(st.clone());
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                                        log::warn!("Soloist WebSocket connection closed or errored; reconnecting in 2s...");
                                        ws_conn_flag.store(false, Ordering::Relaxed);
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    ws_conn_flag.store(false, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(2000)).await;
                }
            }
        }
    });

    // 5. Spotify Web API Task
    let state_spotify = state.clone();
    let req_tx_clone = spotify_req_tx.clone();
    rt.spawn(async move {
        let existing_id = config.client_id.trim().to_string();
        if !existing_id.is_empty() {
            let _ = req_tx_clone.send(SpotifyRequest::Init(existing_id)).await;
        }

        let mut spotify_mgr: Option<SpotifyManager> = None;

        while let Some(req) = spotify_req_rx.recv().await {
            match req {
                SpotifyRequest::Init(client_id) => {
                    match SpotifyManager::init(&client_id).await {
                        Ok(mgr) => {
                            log::info!("Spotify Web API active.");

                            // Query active devices and transfer playback to Soloist preserving current track and progress
                            if let Ok(devs) = mgr.get_available_devices().await {
                                if let Some(soloist_dev) = devs.iter().find(|d| {
                                    let n = d.name.to_lowercase();
                                    n.contains("soloist") || n.contains("spotify-tui")
                                }) {
                                    if !soloist_dev.is_active {
                                        log::info!("Transferring playback to Soloist device: {}", soloist_dev.id);
                                        let _ = mgr.transfer_playback(&soloist_dev.id, true).await;
                                    }
                                }
                            }
                            if let Ok(pls) = mgr.get_user_playlists().await {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.user_playlists = pls;
                                }
                            }
                            if let Ok(albums) = mgr.get_saved_albums().await {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.user_saved_albums = albums;
                                }
                            }
                            if let Ok(artists) = mgr.get_followed_artists().await {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.user_followed_artists = artists;
                                }
                            }
                            if let Ok(user_info) = mgr.get_current_user().await {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.user_name = Some(user_info.0);
                                    st.user_avatar_url = user_info.1;
                                    st.current_user_id = Some(user_info.2); // Store current user ID
                                }
                            }
                            if let Ok(liked) = mgr.get_liked_songs().await {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.liked_track_uris = liked.into_iter().map(|t| t.uri).collect();
                                }
                            }
                            spotify_mgr = Some(mgr);
                        }
                        Err(e) => {
                            log::error!("Spotify Authentication error: {:?}", e);
                            if let Ok(mut st) = state_spotify.write() {
                                st.key_prompt_error = Some(format!("{:?}", e));
                                st.show_key_prompt = true;
                            }
                        }
                    }
                }
                SpotifyRequest::Reauthenticate => {
                    let reauth_result = if let Some(ref mut mgr) = spotify_mgr {
                        mgr.reauthorize().await
                    } else {
                        let stored_id = config.client_id.trim().to_string();
                        if !stored_id.is_empty() {
                            match SpotifyManager::init(&stored_id).await {
                                Ok(m) => {
                                    spotify_mgr = Some(m);
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            Err(anyhow::anyhow!("No client_id found in config"))
                        }
                    };

                    match reauth_result {
                        Ok(()) => {
                            log::info!("Browser re-authorization succeeded. Hydrating user state and library...");
                            if let Some(ref mgr) = spotify_mgr {
                                // 1. User Profile info
                                if let Ok(user_info) = mgr.get_current_user().await {
                                    if let Ok(mut st) = state_spotify.write() {
                                        st.user_name = Some(user_info.0);
                                        st.user_avatar_url = user_info.1;
                                        st.current_user_id = Some(user_info.2);
                                        st.show_key_prompt = false;
                                        st.key_prompt_error = None;
                                    }
                                }
                                // 2. Liked Songs
                                if let Ok(liked) = mgr.get_liked_songs().await {
                                    if let Ok(mut st) = state_spotify.write() {
                                        st.liked_track_uris = liked.iter().map(|t| t.uri.clone()).collect();
                                        if st.active_nav == "Liked Songs" {
                                            st.displayed_tracks = liked;
                                            st.is_loading_view = false;
                                        }
                                    }
                                }
                                // 3. Playlists
                                if let Ok(pls) = mgr.get_user_playlists().await {
                                    if let Ok(mut st) = state_spotify.write() {
                                        st.user_playlists = pls;
                                    }
                                }
                                // 4. Albums & Artists
                                if let Ok(albums) = mgr.get_saved_albums().await {
                                    if let Ok(mut st) = state_spotify.write() {
                                        st.user_saved_albums = albums;
                                    }
                                }
                                if let Ok(artists) = mgr.get_followed_artists().await {
                                    if let Ok(mut st) = state_spotify.write() {
                                        st.user_followed_artists = artists;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("Browser re-authorization failed: {:?}", e);
                            if let Ok(mut st) = state_spotify.write() {
                                st.key_prompt_error = Some(format!("OAuth failed: {:?}", e));
                                st.show_key_prompt = true;
                            }
                        }
                    }
                }
                SpotifyRequest::Search(query) => {
                    if let Some(ref mgr) = spotify_mgr {
                        match mgr.search_tracks(&query).await {
                            Ok(results) => {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.artist_page = None; // <--- Reset artist view
                                    st.search_results = results;
                                    st.displayed_tracks.clear();
                                    st.view_title = format!("Search: \"{}\"", query);
                                }
                            }
                            Err(e) => {
                                log::error!("Search request failed: {:?}", e);
                            }
                        }
                    }
                }
                SpotifyRequest::FetchLikedSongs => {
                    if let Some(ref mgr) = spotify_mgr {
                        match mgr.get_liked_songs().await {
                            Ok(tracks) => {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.artist_page = None;
                                    st.search_results.clear();
                                    st.liked_track_uris = tracks.iter().map(|t| t.uri.clone()).collect();
                                    st.displayed_tracks = tracks;
                                    st.active_context_uri = None;
                                    st.view_title = "Liked Songs".into();
                                    st.is_loading_view = false;
                                }
                            }
                            Err(e) => {
                                log::error!("Failed to fetch liked songs: {:?}", e);
                                if let Ok(mut st) = state_spotify.write() {
                                    st.is_loading_view = false;
                                }
                            }
                        }
                    }
                }
                SpotifyRequest::FetchPlaylist(pl_id) => {
                    if let Some(ref mgr) = spotify_mgr {
                        if let Ok(mut st) = state_spotify.write() {
                            st.artist_page = None;
                            st.search_results.clear();
                            st.displayed_tracks.clear();
                            st.active_context_uri = Some(format!("spotify:playlist:{}", pl_id));
                            st.playlist_total_tracks = None;
                        }

                        let _ = mgr.stream_playlist_tracks(&pl_id, &spotify_resp_tx).await;
                    }
                }
                SpotifyRequest::FetchAlbum(album_id) => {
                    if let Some(ref mgr) = spotify_mgr {
                        match mgr.get_album_tracks(&album_id).await {
                            Ok(tracks) => {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.artist_page = None; // Reset so tracklist shows instead of artist overview
                                    st.search_results.clear();
                                    st.displayed_tracks = tracks;
                                    st.active_context_uri = Some(format!("spotify:album:{}", album_id));
                                }
                            }
                            Err(e) => {
                                log::error!("Failed to fetch album tracks: {:?}", e);
                            }
                        }
                    }
                }
                SpotifyRequest::FetchArtist(artist_id) => {
                    if let Some(ref mgr) = spotify_mgr {
                        if let Ok(page_data) = mgr.get_artist_overview(&artist_id).await {
                            if let Ok(mut st) = state_spotify.write() {
                                st.view_title = page_data.name.clone();
                                st.active_nav = page_data.name.clone();
                                st.search_results.clear();
                                st.displayed_tracks.clear();
                                st.artist_page = Some(page_data);
                                st.active_context_uri = None;
                                st.is_loading_view = false; // Artist data loaded
                            }
                        } else {
                            if let Ok(mut st) = state_spotify.write() {
                                st.is_loading_view = false;
                            }
                        }
                    }
                }
                SpotifyRequest::StartPlayback { uri, context_uri, upcoming_uris } => {
                    if let Some(ref mgr) = spotify_mgr {
                        if let Err(e) = mgr
                            .start_playback(&uri, context_uri.as_deref(), upcoming_uris)
                            .await
                        {
                            log::error!("Web API playback command failed: {:?}", e);
                        }
                    }
                }
                SpotifyRequest::FetchArtistBio(artist_id) => {
                    if let Some(ref mgr) = spotify_mgr {
                        // Check cache quickly and drop lock immediately
                        let is_cached = {
                            state_spotify.read().map(|s| s.artist_info_cache.contains_key(&artist_id)).unwrap_or(false)
                        };

                        if !is_cached {
                            // Network call happens with NO lock held
                            if let Ok(overview) = mgr.get_artist_overview(&artist_id).await {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.artist_info_cache.insert(
                                        artist_id.clone(),
                                        (overview.bio.clone(), overview.followers, overview.monthly_listeners),
                                    );
                                    st.pending_artist_fetches.remove(&artist_id);

                                    if st.current_artist_id.as_deref() == Some(&artist_id) {
                                        st.artist_bio = Some(overview.bio);
                                        st.artist_followers = Some(overview.followers);
                                        st.artist_monthly_listeners = Some(overview.monthly_listeners);
                                    }
                                }
                            } else {
                                // Clean up pending on failure so it can retry later if needed
                                if let Ok(mut st) = state_spotify.write() {
                                    st.pending_artist_fetches.remove(&artist_id);
                                }
                            }
                        } else {
                            if let Ok(mut st) = state_spotify.write() {
                                st.pending_artist_fetches.remove(&artist_id);
                            }
                        }
                    }
                }
                SpotifyRequest::CheckLibraryContext(uri) => {
                    if let Some(ref mgr) = spotify_mgr {
                        match mgr.check_library_contains(&[&uri]).await {
                            Ok(results) => {
                                if let Some(&is_saved) = results.first() {
                                    if let Ok(mut st) = state_spotify.write() {
                                        // Only update if the user is still on the same page
                                        if st.active_context_uri.as_deref() == Some(&uri) {
                                            st.is_context_saved = Some(is_saved);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!("CheckLibraryContext failed for {}: {:?}", uri, e);
                            }
                        }
                    }
                }
                SpotifyRequest::CheckLibraryTrack(uri) => {
                    if let Some(ref mgr) = spotify_mgr {
                        let is_saved = mgr.check_library_contains(&[&uri])
                            .await
                            .map(|v| v.first().copied().unwrap_or(false))
                            .unwrap_or(false);

                        if let Ok(mut st) = state_spotify.write() {
                            st.is_track_saved = is_saved;
                            if is_saved {
                                st.liked_track_uris.insert(uri.clone());
                            } else {
                                st.liked_track_uris.remove(&uri);
                            }

                            // Update active context menu if currently open for this track
                            if let Some(ref mut menu) = st.context_menu {
                                if menu.track.uri == uri {
                                    menu.is_saved = is_saved;
                                }
                            }
                        }
                    }
                }
                SpotifyRequest::ToggleLibraryUri { uri, current_state, is_context } => {
                    if let Some(ref mgr) = spotify_mgr {
                        let next_state = !current_state;

                        // Ensure URI is properly formatted
                        let clean_uri = if uri.starts_with("spotify:") {
                            uri.clone()
                        } else {
                            format!("spotify:track:{}", uri)
                        };

                        let res = if clean_uri.starts_with("spotify:playlist:") {
                            let pl_id = clean_uri.trim_start_matches("spotify:playlist:");
                            let r1 = mgr.set_playlist_followed(pl_id, next_state).await;
                            let r2 = if next_state {
                                mgr.add_to_library(&[&clean_uri]).await
                            } else {
                                mgr.remove_from_library(&[&clean_uri]).await
                            };
                            r1.or(r2)
                        } else if next_state {
                            mgr.add_to_library(&[&clean_uri]).await
                        } else {
                            mgr.remove_from_library(&[&clean_uri]).await
                        };

                        if res.is_ok() {
                            if let Ok(mut st) = state_spotify.write() {
                                if is_context {
                                    st.is_context_saved = Some(next_state);
                                } else {
                                    st.is_track_saved = next_state;
                                    if next_state {
                                        st.liked_track_uris.insert(clean_uri.clone());
                                    } else {
                                        st.liked_track_uris.remove(&clean_uri);
                                    }
                                }
                            }

                            // Auto-refresh the corresponding collection on the left
                            if clean_uri.starts_with("spotify:playlist:") {
                                tokio::time::sleep(Duration::from_millis(350)).await;
                                if let Ok(pls) = mgr.get_user_playlists().await {
                                    if let Ok(mut st) = state_spotify.write() { st.user_playlists = pls; }
                                }
                            } else if clean_uri.starts_with("spotify:artist:") {
                                if let Ok(artists) = mgr.get_followed_artists().await {
                                    if let Ok(mut st) = state_spotify.write() { st.user_followed_artists = artists; }
                                }
                            } else if clean_uri.starts_with("spotify:album:") {
                                if let Ok(albums) = mgr.get_saved_albums().await {
                                    if let Ok(mut st) = state_spotify.write() { st.user_saved_albums = albums; }
                                }
                            }
                        }
                    }
                }
                SpotifyRequest::AddToPlaylist { playlist_id, track_uri } => {
                    if let Some(ref mgr) = spotify_mgr {
                        if let Ok(_) = mgr.add_track_to_playlist(&playlist_id, &track_uri).await {
                            // Automatically reload playlists to update track count badges
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            if let Ok(pls) = mgr.get_user_playlists().await {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.user_playlists = pls;
                                }
                            }
                        }
                    }
                }
                SpotifyRequest::RemoveFromPlaylist { playlist_id, updated_uris } => {
                    if let Some(ref mgr) = spotify_mgr {
                        if let Ok(_) = mgr.remove_track_from_playlist(&playlist_id, &updated_uris).await {
                            // Update sidebar track count
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            if let Ok(pls) = mgr.get_user_playlists().await {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.user_playlists = pls;
                                }
                            }
                        }
                    }
                }
                SpotifyRequest::AddToQueue { track_uri } => {
                    if let Some(ref mgr) = spotify_mgr {
                        let _ = mgr.add_to_queue(&track_uri).await;
                    }
                    // Spotify's daemon will emit a 'queue_changed' WebSocket frame immediately,
                    // which cleanly updates the queue without duplicates.
                }
                SpotifyRequest::RefreshLibrary => {
                    if let Some(ref mgr) = spotify_mgr {
                        if let Err(e) = mgr.ensure_token().await {
                            if e.to_string().contains("SESSION_EXPIRED") {
                                log::warn!("Session expired during library refresh. Triggering browser OAuth re-login...");
                                let _ = req_tx_clone.send(SpotifyRequest::Reauthenticate).await;
                            }
                            continue;
                        }
                        // 1. Refresh Playlists & track counts
                        if let Ok(pls) = mgr.get_user_playlists().await {
                            if let Ok(mut st) = state_spotify.write() {
                                st.user_playlists = pls;
                            }
                        }
                        // 2. Refresh Liked Songs URI cache
                        if let Ok(liked) = mgr.get_liked_songs().await {
                            if let Ok(mut st) = state_spotify.write() {
                                st.liked_track_uris = liked.into_iter().map(|t| t.uri).collect();
                            }
                        }
                        // 3. Refresh Followed Artists
                        if let Ok(artists) = mgr.get_followed_artists().await {
                            if let Ok(mut st) = state_spotify.write() {
                                st.user_followed_artists = artists;
                            }
                        }
                        // 4. Refresh Saved Albums
                        if let Ok(albums) = mgr.get_saved_albums().await {
                            if let Ok(mut st) = state_spotify.write() {
                                st.user_saved_albums = albums;
                            }
                        }
                    }
                }
                SpotifyRequest::FetchDevices => {
                    if let Some(ref mgr) = spotify_mgr {
                        if let Ok(devs) = mgr.get_available_devices().await {
                            if let Ok(mut st) = state_spotify.write() {
                                st.available_devices = devs;
                            }
                        }
                    }
                }
                SpotifyRequest::TransferPlayback { device_id, play } => {
                    if let Some(ref mgr) = spotify_mgr {
                        if let Err(e) = mgr.transfer_playback(&device_id, play).await {
                            log::error!("Failed to transfer playback: {:?}", e);
                        } else {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            if let Ok(devs) = mgr.get_available_devices().await {
                                if let Ok(mut st) = state_spotify.write() {
                                    st.available_devices = devs;
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    // 6. Launch Desktop Window
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([720.0, 480.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(true)
            .with_active(true),
        ..Default::default()
    };


    eframe::run_native(
        "spotify-tui",
        native_options,
        Box::new(|cc| {
            // 1. Install image loaders
            egui_extras::install_image_loaders(&cc.egui_ctx);

            // 2. Configure Unified Font with CJK Fallback
            let mut fonts = egui::FontDefinitions::default();

            // Load Windows system CJK font (Microsoft YaHei covers CJK + Latin cleanly)
            // Alternative paths: "C:\\Windows\\Fonts\\msgothic.ttc" or "C:\\Windows\\Fonts\\malgun.ttf"
            if let Ok(font_data) = std::fs::read("C:\\Windows\\Fonts\\msyh.ttc") {
                fonts.font_data.insert(
                    "cjk_fallback".to_owned(),
                    egui::FontData::from_owned(font_data),
                );

                // Add to Proportional family (at the end as a fallback)
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .push("cjk_fallback".to_owned());

                // Add to Monospace family so code/TUI text renders CJK characters seamlessly
                fonts
                    .families
                    .entry(egui::FontFamily::Monospace)
                    .or_default()
                    .push("cjk_fallback".to_owned());
            } else if let Ok(font_data) = std::fs::read("C:\\Windows\\Fonts\\msgothic.ttc") {
                fonts.font_data.insert(
                    "cjk_fallback".to_owned(),
                    egui::FontData::from_owned(font_data),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Monospace)
                    .or_default()
                    .push("cjk_fallback".to_owned());
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .push("cjk_fallback".to_owned());
            }

            cc.egui_ctx.set_fonts(fonts);

            let mut style = (*cc.egui_ctx.style()).clone();
            for (_text_style, font_id) in style.text_styles.iter_mut() {
                font_id.family = egui::FontFamily::Monospace;
            }

            style.visuals.panel_fill = COLOR_BG;
            style.visuals.window_fill = COLOR_BG;
            style.visuals.extreme_bg_color = COLOR_BG;
            style.visuals.faint_bg_color = COLOR_BG;

            // 1. Force sharp square corners on all interactive states
            style.visuals.widgets.noninteractive.rounding = Rounding::ZERO;
            style.visuals.widgets.inactive.rounding = Rounding::ZERO;
            style.visuals.widgets.hovered.rounding = Rounding::ZERO;
            style.visuals.widgets.active.rounding = Rounding::ZERO;
            style.visuals.widgets.open.rounding = Rounding::ZERO;

            // 2. Ensure menu popups and text selections are square
            style.visuals.window_rounding = Rounding::ZERO;
            style.visuals.menu_rounding = Rounding::ZERO;

            // 3. Disable text selection across all labels and widgets
            style.interaction.selectable_labels = false;
            style.visuals.selection.bg_fill = Color32::TRANSPARENT;
            style.visuals.selection.stroke = Stroke::NONE;

            cc.egui_ctx.set_style(style);

            Ok(Box::new(SoloistApp::new(
                state,
                cmd_tx,
                spotify_req_tx,
                spotify_resp_rx,
                audio_connected,
                ws_connected,
            )))
        }),
    )
        .map_err(|e| anyhow::anyhow!("Eframe launch failed: {:?}", e))?;

    // Stop QEMU when the GUI window is closed
    qemu_backend.stop();
    running.store(false, Ordering::Relaxed);
    Ok(())
}

fn handle_soloist_event(event: &Value, state: &mut AppState) {
    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        "playback_state" | "track_changed" => {
            if let Some(item) = event.get("item") {
                if let Some(uri) = item.get("uri").and_then(|v| v.as_str()) {
                    state.selected_track_uri = Some(uri.to_string());
                }
                // 1. Track Title
                if let Some(name) = item.pointer("/decorations/identity/name")
                    .or_else(|| item.get("name"))
                    .and_then(|v| v.as_str())
                {
                    state.title = name.to_string();
                }

                // 2. Artists: nested under decorations.creators
                let creators_opt = item.pointer("/decorations/creators")
                    .or_else(|| item.get("creators"))
                    .and_then(|v| v.as_array());

                if let Some(creators) = creators_opt {
                    let mut names = Vec::new();
                    let mut pairs = Vec::new();

                    for c in creators {
                        let name = c.pointer("/entity/decorations/identity/name")
                            .or_else(|| c.pointer("/decorations/identity/name"))
                            .or_else(|| c.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        let id = c.pointer("/entity/uri")
                            .or_else(|| c.pointer("/uri"))
                            .and_then(|u| u.as_str())
                            .map(|u| u.trim_start_matches("spotify:artist:").to_string())
                            .unwrap_or_default();

                        if !name.is_empty() {
                            names.push(name.clone());
                            pairs.push((name, id));
                        }
                    }

                    if !names.is_empty() {
                        state.artist = names.join(", ");
                        state.current_artists = pairs;
                    }
                }

                // 3. Album
                if let Some(album) = item.pointer("/decorations/parent/entity/decorations/identity/name")
                    .or_else(|| item.pointer("/parent/entity/decorations/identity/name"))
                    .and_then(|v| v.as_str())
                {
                    state.album = album.to_string();
                }

                // 4. Duration: nested under decorations.playback.duration_ms
                let dur = item.pointer("/decorations/playback/duration_ms")
                    .or_else(|| item.pointer("/playback/duration_ms"))
                    .or_else(|| item.get("duration_ms"))
                    .and_then(|v| v.as_u64());

                if let Some(d) = dur {
                    if d > 0 {
                        state.duration_ms = d;
                    }
                }

                // 5. Album Art
                let covers_opt = item.pointer("/decorations/visual_identity/cover")
                    .or_else(|| item.pointer("/visual_identity/cover"))
                    .and_then(|v| v.as_array());

                if let Some(covers) = covers_opt {
                    if let Some(url) = covers.iter()
                        .find(|c| c.get("size").and_then(|s| s.as_str()) == Some("large"))
                        .or_else(|| covers.iter().find(|c| c.get("size").and_then(|s| s.as_str()) == Some("xlarge")))
                        .or_else(|| covers.first())
                        .and_then(|c| c.get("url"))
                        .and_then(|u| u.as_str())
                    {
                        state.cover_url = Some(url.to_string());
                    }
                }
                let creator_id = creators_opt.and_then(|c| {
                    c.first().and_then(|creator| {
                        creator.pointer("/entity/uri")
                            .or_else(|| creator.pointer("/uri"))
                            .and_then(|u| u.as_str())
                            .map(|u| u.trim_start_matches("spotify:artist:").to_string())
                    })
                });

                if let Some(id) = creator_id {
                    if state.current_artist_id.as_deref() != Some(&id) {
                        state.current_artist_id = Some(id.clone());
                        // Check cache immediately
                        if let Some((bio, followers, monthly)) = state.artist_info_cache.get(&id) {
                            state.artist_bio = Some(bio.clone());
                            state.artist_followers = Some(*followers);
                            state.artist_monthly_listeners = Some(*monthly);
                        } else {
                            state.artist_bio = None;
                            state.artist_followers = None;
                            state.artist_monthly_listeners = None;
                        }
                    }
                }
                if let Some(album_uri) = item.pointer("/decorations/parent/entity/uri")
                    .or_else(|| item.pointer("/parent/entity/uri"))
                    .and_then(|v| v.as_str())
                {
                    state.current_album_id = Some(album_uri.trim_start_matches("spotify:album:").to_string());
                }
            }

            // Play / Pause status
            if let Some(status) = event.get("status").and_then(|v| v.as_str()) {
                state.is_playing = status == "playing";
            }

            // Position & Speed
            if let Some(pos_obj) = event.get("position") {
                if let Some(pos) = pos_obj.get("position_ms").and_then(|v| v.as_u64()) {
                    state.position_ms = pos;
                    state.last_sync = Instant::now();
                }
                if let Some(spd) = pos_obj.get("speed").and_then(|v| v.as_f64()) {
                    state.playback_speed = spd;
                    state.is_playing = spd > 0.0;
                }
            }
            // Volume
            if let Some(vol) = event.get("volume").and_then(|v| v.as_u64()) {
                state.volume = vol.clamp(0, 100) as u8;
            }
        }

        "volume_changed" => {
            if let Some(vol) = event.get("volume").and_then(|v| v.as_u64()) {
                state.volume = vol.clamp(0, 100) as u8;
            }
        }

        "position_sync" => {
            if let Some(pos_obj) = event.get("position") {
                if let Some(pos) = pos_obj.get("position_ms").and_then(|v| v.as_u64()) {
                    state.position_ms = pos;
                    state.last_sync = Instant::now();
                }
                if let Some(spd) = pos_obj.get("speed").and_then(|v| v.as_f64()) {
                    state.playback_speed = spd;
                    state.is_playing = spd > 0.0;
                }
            } else if let Some(pos) = event.get("position_ms").and_then(|v| v.as_u64()) {
                state.position_ms = pos;
                state.last_sync = Instant::now();
            }
        }

        "options_changed" => {
            if let Some(options) = event.get("options") {
                if let Some(shuffle) = options.get("shuffle").and_then(|v| v.as_bool()) {
                    state.shuffle_state = shuffle;
                }
                if let Some(repeat_str) = options.get("repeat").and_then(|v| v.as_str()) {
                    state.repeat_state = match repeat_str {
                        "track" => 2,
                        "context" | "all" => 1,
                        _ => 0,
                    };
                }
            }
        }

        "queue_changed" | "queue_state" | "queue" => {
            let mut manual_tracks = Vec::new();
            let mut upcoming_tracks = Vec::new();

            let parse_single_item = |entry: &serde_json::Value| -> Option<(SpotifyTrackItem, bool)> {
                let item = entry.get("item").unwrap_or(entry);
                let title = item.pointer("/decorations/identity/name")
                    .or_else(|| item.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if title.is_empty() { return None; }

                let uri = item.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let duration_ms = item.pointer("/playback/duration_ms")
                    .or_else(|| item.pointer("/decorations/playback/duration_ms"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let album = item.pointer("/decorations/parent/entity/decorations/identity/name")
                    .or_else(|| item.pointer("/parent/entity/decorations/identity/name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Album")
                    .to_string();

                let creators_opt = item.pointer("/decorations/creators")
                    .or_else(|| item.get("creators"))
                    .and_then(|v| v.as_array());

                let album_id = item.pointer("/decorations/parent/entity/uri")
                    .or_else(|| item.pointer("/parent/entity/uri"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim_start_matches("spotify:album:").to_string());

                let mut artist_pairs = Vec::new();
                let artists_str = if let Some(creators) = creators_opt {
                    let mut names = Vec::new();
                    for c in creators {
                        let name = c.pointer("/entity/decorations/identity/name")
                            .or_else(|| c.pointer("/decorations/identity/name"))
                            .or_else(|| c.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let id = c.pointer("/entity/uri")
                            .or_else(|| c.pointer("/uri"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim_start_matches("spotify:artist:").to_string())
                            .unwrap_or_default();
                        if !name.is_empty() {
                            names.push(name.clone());
                            artist_pairs.push((name, id));
                        }
                    }
                    names.join(", ")
                } else {
                    "Unknown Artist".to_string()
                };

                // In Spotify protocol:
                // provider == "queue" or provider == "user" or has queued_by indicates a manually queued track.
                let provider = entry.get("provider")
                    .or_else(|| item.get("provider"))
                    .or_else(|| entry.pointer("/metadata/provider"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let is_queued = provider == "queue"
                    || provider == "user"
                    || entry.pointer("/metadata/is_queued").and_then(|v| v.as_bool()).unwrap_or(false)
                    || item.pointer("/metadata/is_queued").and_then(|v| v.as_bool()).unwrap_or(false)
                    || entry.get("queued_by").is_some()
                    || item.get("queued_by").is_some();

                Some((
                    SpotifyTrackItem {
                        title,
                        artist: artists_str,
                        album,
                        duration_ms,
                        uri,
                        artists: artist_pairs,
                        album_id,
                        uid: None,
                    },
                    is_queued,
                ))
            };

            // 1. Check if the WebSocket payload provides a separate "queue" property
            if let Some(queue_arr) = event.get("queue").and_then(|v| v.as_array()) {
                for entry in queue_arr {
                    if let Some((t, _)) = parse_single_item(entry) {
                        manual_tracks.push(t);
                    }
                }
            }

            // 2. Classify items inside "upcoming"
            if let Some(upcoming_arr) = event.get("upcoming").and_then(|v| v.as_array()) {
                for entry in upcoming_arr {
                    if let Some((t, is_queued)) = parse_single_item(entry) {
                        if is_queued {
                            manual_tracks.push(t);
                        } else {
                            upcoming_tracks.push(t);
                        }
                    }
                }
            }

            state.manual_queue_items = manual_tracks;
            state.next_up_items = upcoming_tracks;
        }

        _ => {}
    }
}

fn run_smtc_service(
    cmd_tx: mpsc::Sender<SoloistCommand>,
    meta_rx: std::sync::mpsc::Receiver<AppState>,
) -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("Soloist SMTC Host"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            instance,
            None,
        )?;

        let config = PlatformConfig {
            dbus_name: "soloist",
            display_name: "Spotify Soloist Lossless",
            hwnd: Some(hwnd.0 as *mut c_void),
        };

        let mut controls = MediaControls::new(config)
            .map_err(|e| anyhow::anyhow!("Failed to initialize MediaControls: {:?}", e))?;

        let tx = cmd_tx.clone();
        controls
            .attach(move |event: MediaControlEvent| {
                let _ = match event {
                    MediaControlEvent::Play => tx.blocking_send(SoloistCommand::Play),
                    MediaControlEvent::Pause => tx.blocking_send(SoloistCommand::Pause),
                    MediaControlEvent::Toggle => tx.blocking_send(SoloistCommand::Toggle),
                    MediaControlEvent::Next => tx.blocking_send(SoloistCommand::Next),
                    MediaControlEvent::Previous => tx.blocking_send(SoloistCommand::Previous),
                    _ => Ok(()),
                };
            })
            .map_err(|e| anyhow::anyhow!("Failed to attach SMTC: {:?}", e))?;

        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            while let Ok(info) = meta_rx.try_recv() {
                let _ = controls.set_metadata(MediaMetadata {
                    title: Some(&info.title),
                    artist: Some(&info.artist),
                    album: Some(&info.album),
                    cover_url: info.cover_url.as_deref(),
                    duration: Some(Duration::from_millis(info.duration_ms)),
                });

                let playback = if info.is_playing {
                    MediaPlayback::Playing {
                        progress: Some(souvlaki::MediaPosition(Duration::from_millis(info.position_ms))),
                    }
                } else {
                    MediaPlayback::Paused {
                        progress: Some(souvlaki::MediaPosition(Duration::from_millis(info.position_ms))),
                    }
                };
                let _ = controls.set_playback(playback);
            }

            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn run_wasapi_shared_renderer(mut consumer: Consumer<u8>) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("CoInitializeEx failed")?;

        let mut task_index = 0u32;
        let mmcss_task = windows::core::w!("Audio");
        let mmcss_handle = AvSetMmThreadCharacteristicsW(mmcss_task, &mut task_index);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("Failed to create IMMDeviceEnumerator")?;

        let device: IMMDevice = enumerator
            .GetDefaultAudioEndpoint(eRender, eMultimedia)
            .context("Failed to get default audio render endpoint")?;

        let audio_client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("Failed to activate IAudioClient")?;

        let mix_format_ptr = audio_client
            .GetMixFormat()
            .context("Failed to query device mix format")?;
        let mix_format = &*mix_format_ptr;

        let target_rate = mix_format.nSamplesPerSec as usize;
        let target_channels = mix_format.nChannels as usize;

        // Include stream routing flags for virtual mixers (SteelSeries Sonar)
        let stream_flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        audio_client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                stream_flags,
                0,
                0,
                mix_format_ptr,
                Some(std::ptr::null()),
            )
            .context("Failed to initialize IAudioClient in Shared Mode")?;

        let buffer_event = CreateEventW(None, false, false, None)
            .context("Failed to create buffer event")?;
        audio_client
            .SetEventHandle(buffer_event)
            .context("Failed to set event handle")?;

        let buffer_frames = audio_client
            .GetBufferSize()
            .context("Failed to get buffer size")? as usize;

        let render_client: IAudioRenderClient = audio_client
            .GetService()
            .context("Failed to get IAudioRenderClient")?;

        let chunk_in_size = 1024;
        let mut resampler = FastFixedIn::<f32>::new(
            target_rate as f64 / GUEST_SAMPLE_RATE as f64,
            1.0,
            rubato::PolynomialDegree::Cubic,
            chunk_in_size,
            2,
        ).context("Failed to create resampler")?;

        while consumer.slots() < PREBUFFER_BYTES {
            thread::sleep(Duration::from_millis(10));
        }

        audio_client.Start().context("Failed to start audio client")?;

        let mut resampler_in = vec![vec![0.0f32; chunk_in_size]; 2];
        let mut resampled_out_queue: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
        let mut raw_bytes_buf = vec![0u8; chunk_in_size * BYTES_PER_FRAME];

        let scale = 1.0f32 / 8_388_608.0f32;

        loop {
            let wait_res = WaitForSingleObject(buffer_event, 2000);
            if wait_res != WAIT_OBJECT_0 {
                continue;
            }

            let padding_frames = audio_client.GetCurrentPadding()? as usize;
            let available_frames = buffer_frames.saturating_sub(padding_frames);

            if available_frames == 0 {
                continue;
            }

            while resampled_out_queue[0].len() < available_frames {
                if consumer.slots() < raw_bytes_buf.len() {
                    break;
                }

                let mut read = 0;
                while read < raw_bytes_buf.len() {
                    if let Ok(chunk) = consumer.read_chunk(raw_bytes_buf.len() - read) {
                        let (first, second) = chunk.as_slices();
                        let f_len = first.len();
                        let s_len = second.len();
                        raw_bytes_buf[read..read + f_len].copy_from_slice(first);
                        if s_len > 0 {
                            raw_bytes_buf[read + f_len..read + f_len + s_len].copy_from_slice(second);
                        }
                        let total_read = f_len + s_len;
                        chunk.commit_all();
                        read += total_read;
                    }
                }

                for i in 0..chunk_in_size {
                    let base = i * 6;
                    let left = i32::from_le_bytes([
                        raw_bytes_buf[base],
                        raw_bytes_buf[base + 1],
                        raw_bytes_buf[base + 2],
                        if raw_bytes_buf[base + 2] & 0x80 != 0 { 0xFF } else { 0x00 },
                    ]);
                    let right = i32::from_le_bytes([
                        raw_bytes_buf[base + 3],
                        raw_bytes_buf[base + 4],
                        raw_bytes_buf[base + 5],
                        if raw_bytes_buf[base + 5] & 0x80 != 0 { 0xFF } else { 0x00 },
                    ]);

                    resampler_in[0][i] = left as f32 * scale;
                    resampler_in[1][i] = right as f32 * scale;
                }

                if let Ok(out) = resampler.process(&resampler_in, None) {
                    resampled_out_queue[0].extend_from_slice(&out[0]);
                    resampled_out_queue[1].extend_from_slice(&out[1]);
                }
            }

            let dest_buffer_ptr = match render_client.GetBuffer(available_frames as u32) {
                Ok(ptr) => ptr,
                Err(_) => break,
            };

            if resampled_out_queue[0].len() >= available_frames {
                let out_slice = std::slice::from_raw_parts_mut(
                    dest_buffer_ptr as *mut f32,
                    available_frames * target_channels,
                );

                for f in 0..available_frames {
                    let out_idx = f * target_channels;
                    out_slice[out_idx] = resampled_out_queue[0][f];
                    out_slice[out_idx + 1] = resampled_out_queue[1][f];

                    for ch in 2..target_channels {
                        out_slice[out_idx + ch] = 0.0;
                    }
                }

                resampled_out_queue[0].drain(0..available_frames);
                resampled_out_queue[1].drain(0..available_frames);

                render_client.ReleaseBuffer(available_frames as u32, 0)?;
            } else {
                render_client.ReleaseBuffer(
                    available_frames as u32,
                    AUDCLNT_BUFFERFLAGS_SILENT.0 as u32,
                )?;
            }
        }

        let _ = audio_client.Stop();
        if let Ok(handle) = mmcss_handle {
            let _ = AvRevertMmThreadCharacteristics(handle);
        }
        CoTaskMemFree(Some(mix_format_ptr as *const _));
        CoUninitialize();
    }
    Ok(())
}