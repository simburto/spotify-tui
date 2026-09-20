use anyhow::{Context, Result};
use rspotify::{
    prelude::*,
    scopes, AuthCodePkceSpotify, Config, Credentials, OAuth,
};
use rspotify::model::{
    AlbumId, Offset, PlayContextId, PlayableId, PlaylistId, TrackId,
};
use serde_json::Value;
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, Default)]
pub struct SpotifyTrackItem {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub uri: String,
    pub artists: Vec<(String, String)>, // <-- Add this field
    pub album_id: Option<String>,
    pub uid: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SpotifyPlaylistItem {
    pub name: String,
    pub id: String,
    pub tracks_total: u32,
    pub uri: String,
    pub owner_id: String,
}

#[derive(Debug, Clone)]
pub enum SearchResultKind {
    Track,
    Artist,
    Album,
    Playlist,
}

#[derive(Debug, Clone)]
pub struct SearchResultItem {
    pub kind: SearchResultKind,
    pub title: String,
    pub subtitle: String,
    pub uri: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ArtistReleaseItem {
    pub name: String,
    pub uri: String,
    pub release_type: String,
    pub year: Option<u64>,
    pub track_count: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ArtistConcertItem {
    pub title: String,
    pub city: String,
    pub venue: String,
    pub date: String,
    pub uri: String,
}

#[derive(Debug, Clone, Default)]
pub struct ArtistPageData {
    pub id: String,
    pub name: String,
    pub bio: String,
    pub followers: u64,
    pub monthly_listeners: u64,
    pub top_cities: Vec<(String, String, u64)>, // (City, Country, Listeners)
    pub top_tracks: Vec<SpotifyTrackItem>,
    pub albums: Vec<ArtistReleaseItem>,
    pub singles: Vec<ArtistReleaseItem>,
    pub compilations: Vec<ArtistReleaseItem>,
    pub appears_on: Vec<ArtistReleaseItem>,
    pub artist_playlists: Vec<SpotifyPlaylistItem>,
    pub featuring_playlists: Vec<SpotifyPlaylistItem>,
    pub discovered_on: Vec<SpotifyPlaylistItem>,
    pub related_artists: Vec<(String, String)>, // (Name, URI)
    pub concerts: Vec<ArtistConcertItem>,
}

#[derive(Debug, Clone, Default)]
pub struct SpotifyFollowedArtistItem {
    pub name: String,
    pub id: String,
    pub uri: String,
}

#[derive(Debug, Clone, Default)]
pub struct SpotifySavedAlbumItem {
    pub name: String,
    pub id: String,
    pub artist: String,
    pub uri: String,
}

pub struct SpotifyManager {
    client: AuthCodePkceSpotify,
}

#[derive(Debug, Clone)]
pub enum SpotifyResponse {
    PlaylistInitial {
        playlist_id: String,
        tracks: Vec<SpotifyTrackItem>,
        total: usize,
    },
    PlaylistChunk {
        playlist_id: String,
        tracks: Vec<SpotifyTrackItem>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct SpotifyDeviceItem {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub is_active: bool,
    pub volume_percent: Option<u32>,
}

impl SpotifyManager {
    pub async fn init(client_id: &str) -> Result<Self> {
        let redirect_uri = "http://127.0.0.1:8888/callback".to_string();

        let oauth = OAuth {
            redirect_uri,
            scopes: scopes!(
                "user-read-private",
                "user-read-playback-state",
                "user-modify-playback-state",
                "playlist-read-private",
                "playlist-read-collaborative",
                "playlist-modify-public",
                "playlist-modify-private",
                "user-library-read",
                "user-library-modify", // <--- ADD THIS
                "user-follow-read",
                "user-follow-modify",  // <--- ADD THIS
                "user-read-recently-played",
                "user-top-read"
            ),
            ..Default::default()
        };

        let credentials = Credentials::new_pkce(client_id);
        let config = Config {
            token_cached: true,
            cache_path: std::path::PathBuf::from(".spotify_token_cache.json"),
            ..Default::default()
        };

        let mut spotify = AuthCodePkceSpotify::with_config(credentials, oauth, config);

        let url = spotify.get_authorize_url(None)?;
        match spotify.read_token_cache(false).await {
            Ok(Some(token)) => {
                log::info!("Loaded cached Spotify token from disk.");
                *spotify.token.lock().await.unwrap() = Some(token);
            }
            _ => {
                log::info!("Starting temporary local server on 127.0.0.1:8888 for OAuth callback...");

                // Bind a single-use TCP listener on the exact redirect URI port
                let listener = tokio::net::TcpListener::bind("127.0.0.1:8888")
                    .await
                    .context("Failed to bind 127.0.0.1:8888 for OAuth callback")?;

                log::info!("Opening browser for Spotify OAuth authorization...");
                let _ = open::that(&url);

                // Wait for the browser redirect hit
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .context("Failed to accept OAuth callback connection")?;

                let mut buffer = [0u8; 2048];
                let bytes_read = socket
                    .read(&mut buffer)
                    .await
                    .context("Failed to read HTTP request from callback")?;

                let request = String::from_utf8_lossy(&buffer[..bytes_read]);

                // Parse request line: "GET /callback?code=... HTTP/1.1"
                let target_path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .context("Malformed HTTP request line from callback")?;

                // Build the full callback URL for rspotify to parse code & state
                let full_callback_url = format!("http://127.0.0.1:8888{}", target_path);

                // Extract authorization code and fetch token
                let code = spotify
                    .parse_response_code(&full_callback_url)
                    .context("Failed to parse authorization code from callback URL")?;

                spotify
                    .request_token(&code)
                    .await
                    .context("Failed to exchange OAuth authorization code for access token")?;

                // Send a friendly success response to the browser window
                let response_body = r#"<!DOCTYPE html>
                    <html>
                    <head><title>Spotify-tui Authorization Successful</title></head>
                    <body style="background-color: #191724; color: #ebbcba; font-family: monospace; display: flex; flex-direction: column; align-items: center; justify-content: center; height: 80vh;">
                        <h2>Authentication Successful!</h2>
                        <p style="color: #908caa;">Spotify-tui has received your Spotify credentials. You can safely close this tab.</p>
                    </body>
                    </html>"#;

                let http_response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );

                use tokio::io::AsyncWriteExt;
                let _ = socket.write_all(http_response.as_bytes()).await;
                let _ = socket.flush().await;

                // Cache token to disk
                let _ = spotify.write_token_cache().await;
                log::info!("Spotify token successfully captured and written to disk cache.");
            }
        }

        Ok(Self { client: spotify })
    }

    /// Searches Spotify catalog via Pathfinder v2, ordered by Spotify's relevance/topResults
    pub async fn search_tracks(&self, query: &str) -> Result<Vec<SearchResultItem>> {
        log::info!("Searching Spotify via Pathfinder v2 for: '{}'", query);

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .build()?;

        // 1. Retrieve user's authenticated token from the active session
        let mut access_token = {
            let token_lock = self.client.token.lock().await.unwrap();
            token_lock.as_ref().map(|t| t.access_token.clone()).unwrap_or_default()
        };

        // Fallback to embed scrape only if no user token exists
        if access_token.is_empty() {
            if let Ok(resp) = client.get("https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M").send().await {
                if let Ok(text) = resp.text().await {
                    let marker = r#""accessToken":""#;
                    if let Some(start) = text.find(marker) {
                        let start_idx = start + marker.len();
                        if let Some(end_idx) = text[start_idx..].find('"') {
                            access_token = text[start_idx..start_idx + end_idx].to_string();
                        }
                    }
                }
            }
        }
        let mut client_token = String::new();

        // 1. Session Token
        if let Ok(resp) = client.get("https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M").send().await {
            if let Ok(text) = resp.text().await {
                let marker = r#""accessToken":""#;
                if let Some(start) = text.find(marker) {
                    let start_idx = start + marker.len();
                    if let Some(end_idx) = text[start_idx..].find('"') {
                        access_token = text[start_idx..start_idx + end_idx].to_string();
                    }
                }
            }
        }

        // 2. Anonymous client-token
        let ct_payload = serde_json::json!({
            "client_data": {
                "client_version": "1.2.40.584.g90c6665a",
                "client_id": "d8a5dc950d20472e8470d8641113735f",
                "js_sdk_data": {
                    "device_brand": "unknown",
                    "device_model": "desktop",
                    "os": "Windows",
                    "os_version": "NT 10.0"
                }
            }
        });

        if let Ok(resp) = client
            .post("https://clienttoken.spotify.com/v1/clienttoken")
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("Origin", "https://open.spotify.com")
            .header("Referer", "https://open.spotify.com/")
            .json(&ct_payload)
            .send()
            .await
        {
            if let Ok(ct_json) = resp.json::<Value>().await {
                if let Some(tok) = ct_json.pointer("/granted_token/token").and_then(|v| v.as_str()) {
                    client_token = tok.to_string();
                }
            }
        }

        if access_token.is_empty() {
            anyhow::bail!("Failed to acquire active session token for search");
        }

        // 3. Dispatch Pathfinder searchDesktop
        let operation_hash = "1148393611bbc58e84e47aed35ecc731275df9f9eb660956962e352dd3631d89";
        let gql_url = "https://api-partner.spotify.com/pathfinder/v2/query";

        let body = serde_json::json!({
            "operationName": "searchDesktop",
            "variables": {
                "searchTerm": query,
                "offset": 0,
                "limit": 20,
                "numberOfTopResults": 5,
                "includeAudiobooks": false,
                "includeAlbumPreReleases": false,
                "includeArtistHasConcertsField": false,
                "includeAuthors": false,
                "includeEpisodeContentRatingsV2": true,
                "includePreReleases": true,
                "isPrefix": null,
                "sectionFilters": ["GENERIC"]
            },
            "extensions": {
                "persistedQuery": {
                    "version": 1,
                    "sha256Hash": operation_hash
                }
            }
        });

        let mut req = client
            .post(gql_url)
            .bearer_auth(&access_token)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("app-platform", "WebPlayer")
            .header("spotify-app-version", "1.2.40.584.g90c6665a")
            .header("Origin", "https://open.spotify.com")
            .header("Referer", "https://open.spotify.com/")
            .json(&body);

        if !client_token.is_empty() {
            req = req.header("client-token", &client_token);
        }

        let resp = req.send().await.context("Pathfinder search network request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Pathfinder search returned HTTP {}: {}", status, err_body);
        }

        let val: Value = resp.json().await.context("Failed to parse search response JSON")?;
        // 5. Build Spotify-style blended list

        // Helper to parse entity items uniformly
        let parse_item = |data: &Value| -> Option<SearchResultItem> {
            let uri = data.get("uri").and_then(|v| v.as_str())?.to_string();
            let typename = data.get("__typename").and_then(|v| v.as_str()).unwrap_or("");

            if uri.starts_with("spotify:artist:") || typename == "Artist" {
                let name = data.pointer("/profile/name").or_else(|| data.get("name")).and_then(|v| v.as_str())?.to_string();
                Some(SearchResultItem {
                    kind: SearchResultKind::Artist,
                    title: name,
                    subtitle: "Artist".to_string(),
                    uri,
                    duration_ms: 0,
                })
            } else if uri.starts_with("spotify:track:") || typename == "Track" {
                let name = data.get("name").and_then(|v| v.as_str())?.to_string();
                let album = data.pointer("/albumOfTrack/name").or_else(|| data.pointer("/album/name")).and_then(|v| v.as_str()).unwrap_or("Unknown Album");
                let artists = if let Some(arr) = data.pointer("/artists/items").and_then(|v| v.as_array()) {
                    arr.iter().filter_map(|a| a.pointer("/profile/name").or_else(|| a.get("name")).and_then(|n| n.as_str())).collect::<Vec<_>>().join(", ")
                } else {
                    "Unknown Artist".to_string()
                };
                let duration_ms = data.pointer("/trackDuration/totalMilliseconds").and_then(|v| v.as_u64()).unwrap_or(0);
                Some(SearchResultItem {
                    kind: SearchResultKind::Track,
                    title: name,
                    subtitle: format!("{} • {}", artists, album),
                    uri,
                    duration_ms,
                })
            } else if uri.starts_with("spotify:album:") || typename == "Album" {
                let name = data.get("name").and_then(|v| v.as_str())?.to_string();
                let artist = data.pointer("/artists/items/0/profile/name").and_then(|v| v.as_str()).unwrap_or("Various Artists");
                Some(SearchResultItem {
                    kind: SearchResultKind::Album,
                    title: name,
                    subtitle: format!("Album • {}", artist),
                    uri,
                    duration_ms: 0,
                })
            } else if uri.starts_with("spotify:playlist:") || typename == "Playlist" {
                let name = data.get("name").and_then(|v| v.as_str())?.to_string();
                let owner = data.pointer("/ownerV2/data/name").and_then(|v| v.as_str()).unwrap_or("Spotify");
                Some(SearchResultItem {
                    kind: SearchResultKind::Playlist,
                    title: name,
                    subtitle: format!("Playlist • By {}", owner),
                    uri,
                    duration_ms: 0,
                })
            } else {
                None
            }
        };

        // 4. Extract categories into temporary buckets
        let mut top_results = Vec::new();
        let mut track_results = Vec::new();
        let mut artist_results = Vec::new();
        let mut album_results = Vec::new();
        let mut playlist_results = Vec::new();

        // Top match / featured
        if let Some(arr) = val.pointer("/data/searchV2/topResults/items")
            .or_else(|| val.pointer("/data/searchV2/topResultsV2/items"))
            .and_then(|v| v.as_array())
        {
            for item in arr {
                let target = item.pointer("/item/data").or_else(|| item.pointer("/data")).unwrap_or(item);
                if let Some(res) = parse_item(target) {
                    top_results.push(res);
                }
            }
        }

        // Tracks
        if let Some(arr) = val.pointer("/data/searchV2/tracksV2/items")
            .or_else(|| val.pointer("/data/searchV2/tracks/items"))
            .and_then(|v| v.as_array())
        {
            for item in arr {
                let target = item.pointer("/item/data").or_else(|| item.pointer("/itemV2/data")).unwrap_or(item);
                if let Some(res) = parse_item(target) {
                    track_results.push(res);
                }
            }
        }

        // Artists
        if let Some(arr) = val.pointer("/data/searchV2/artists/items")
            .or_else(|| val.pointer("/data/searchV2/artistsV2/items"))
            .and_then(|v| v.as_array())
        {
            for item in arr {
                let target = item.pointer("/data").unwrap_or(item);
                if let Some(res) = parse_item(target) {
                    artist_results.push(res);
                }
            }
        }

        // Albums
        if let Some(arr) = val.pointer("/data/searchV2/albums/items")
            .or_else(|| val.pointer("/data/searchV2/albumsV2/items"))
            .and_then(|v| v.as_array())
        {
            for item in arr {
                let target = item.pointer("/data").unwrap_or(item);
                if let Some(res) = parse_item(target) {
                    album_results.push(res);
                }
            }
        }

        // Playlists
        if let Some(arr) = val.pointer("/data/searchV2/playlists/items")
            .or_else(|| val.pointer("/data/searchV2/playlistsV2/items"))
            .and_then(|v| v.as_array())
        {
            for item in arr {
                let target = item.pointer("/data").unwrap_or(item);
                if let Some(res) = parse_item(target) {
                    playlist_results.push(res);
                }
            }
        }

        // 5. Build Spotify-style blended list
        let mut results = Vec::new();
        let mut seen_uris = std::collections::HashSet::new();

        let mut push_item = |item: SearchResultItem| {
            if seen_uris.insert(item.uri.clone()) {
                results.push(item);
            }
        };

        // 1. Top Results first (highest relevance match)
        for item in top_results {
            push_item(item);
        }

        // 2. Interleave tracks, artists, albums, and playlists by ranking
        let mut t_iter = track_results.into_iter();
        let mut ar_iter = artist_results.into_iter();
        let mut al_iter = album_results.into_iter();
        let mut pl_iter = playlist_results.into_iter();

        // Push top 4 primary songs
        for _ in 0..4 {
            if let Some(item) = t_iter.next() { push_item(item); }
        }

        // Interleave remaining items
        loop {
            let mut added = false;

            if let Some(item) = ar_iter.next() { push_item(item); added = true; }
            if let Some(item) = t_iter.next() { push_item(item); added = true; }
            if let Some(item) = al_iter.next() { push_item(item); added = true; }
            if let Some(item) = pl_iter.next() { push_item(item); added = true; }
            if let Some(item) = t_iter.next() { push_item(item); added = true; }

            if !added {
                break;
            }
        }

        log::info!("Blended search returned {} interleaved items", results.len());

        // Fallback to official Web API if Pathfinder schema/token returned 0 items
        if results.is_empty() {
            log::warn!("Pathfinder returned 0 results; falling back to official Web API search.");
            if let Ok(search_res) = self.client.search(
                query,
                rspotify::model::SearchType::Track,
                None,
                None,
                Some(20),
                Some(0),
            ).await {
                if let rspotify::model::SearchResult::Tracks(page) = search_res {
                    for track in page.items {
                        let artists = track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
                        let duration_ms = track.duration.num_milliseconds() as u64;
                        let uri = track.id.map(|id| id.uri()).unwrap_or_default();

                        results.push(SearchResultItem {
                            kind: SearchResultKind::Track,
                            title: track.name,
                            subtitle: format!("{} • {}", artists, track.album.name),
                            uri,
                            duration_ms,
                        });
                    }
                }
            }
        }

        Ok(results)
    }

    /// Fetches user playlists using resilient JSON parsing
    pub async fn get_user_playlists(&self) -> Result<Vec<SpotifyPlaylistItem>> {
        log::info!("Fetching user playlists from Spotify Web API...");

        let raw_json: String = self
            .client
            .api_get("me/playlists?limit=50&offset=0", &Default::default())
            .await
            .context("Failed to query me/playlists endpoint")?;

        let val: Value = serde_json::from_str(&raw_json)
            .context("Failed to parse me/playlists JSON response")?;

        let mut playlists = Vec::new();

        if let Some(items) = val.get("items").and_then(|v| v.as_array()) {
            for item in items {
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Untitled Playlist")
                    .to_string();
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let uri = item.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();

                let tracks_total = item
                    .pointer("/items/total")
                    .or_else(|| item.pointer("/tracks/total"))
                    .or_else(|| item.pointer("/total"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;

                let owner_id = item
                    .pointer("/owner/id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                if !id.is_empty() {
                    playlists.push(SpotifyPlaylistItem {
                        name,
                        id,
                        tracks_total,
                        uri,
                        owner_id,
                    });
                }
            }
        }

        log::info!("Successfully parsed {} user playlists", playlists.len());
        Ok(playlists)
    }

    pub async fn stream_playlist_tracks(
        &self,
        playlist_id: &str,
        resp_tx: &tokio::sync::mpsc::Sender<SpotifyResponse>,
    ) -> Result<()> {
        log::info!("Streaming playlist tracks for ID: {}", playlist_id);

        match Self::fetch_public_embed_tracks(playlist_id, resp_tx).await {
            Ok(true) => {
                log::info!("Successfully streamed tracks via Pathfinder.");
                return Ok(());
            }
            _ => log::warn!("Pathfinder stream yielded 0 items. Falling back to authenticated private endpoint..."),
        }

        self.fetch_private_playlist_tracks(playlist_id, resp_tx).await
    }

    /// Fetches user-owned/private playlist tracks using the updated Web API `/items` endpoint
    async fn fetch_private_playlist_tracks(
        &self,
        playlist_id: &str,
        resp_tx: &tokio::sync::mpsc::Sender<SpotifyResponse>,
    ) -> Result<()> {
        self.ensure_token().await?;
        let mut offset = 0;
        let limit = 50;
        let mut first_batch = true;

        loop {
            let endpoint = format!("playlists/{}/items?limit={}&offset={}", playlist_id, limit, offset);

            let raw_json: String = match self.client.api_get(&endpoint, &Default::default()).await {
                Ok(j) => j,
                Err(e) => {
                    let fallback_endpoint = format!("playlists/{}/tracks?limit={}&offset={}", playlist_id, limit, offset);
                    match self.client.api_get(&fallback_endpoint, &Default::default()).await {
                        Ok(j) => j,
                        Err(_) => {
                            log::warn!("Web API playlist query rejected at offset {}: {:?}", offset, e);
                            break;
                        }
                    }
                }
            };

            let val: serde_json::Value = serde_json::from_str(&raw_json).unwrap_or_default();
            let total = val.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let items = match val.get("items").and_then(|v| v.as_array()) {
                Some(arr) if !arr.is_empty() => arr,
                _ => break,
            };

            let page_count = items.len();
            let mut page_tracks = Vec::new();

            for item in items {
                let track_obj = item.get("track").or_else(|| item.get("item"));
                if let Some(t) = track_obj {
                    if t.is_null() { continue; }

                    let title = t.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if title.is_empty() { continue; }

                    let uri = t.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    let duration_ms = t.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                    let album = t.pointer("/album/name").and_then(|v| v.as_str()).unwrap_or("Unknown Album").to_string();

                    // ---> INSERT HERE: Extract uid <---
                    let uid = item.get("uid")
                        .or_else(|| item.get("track_uid"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let album_id = t.pointer("/album/id")
                        .or_else(|| t.pointer("/album/uri"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim_start_matches("spotify:album:").to_string());

                    let (artist_names, artists_vec) = if let Some(arr) = t.get("artists").and_then(|v| v.as_array()) {
                        let mut names = Vec::new();
                        let mut pairs = Vec::new();
                        for a in arr {
                            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let id = a.get("id")
                                .or_else(|| a.get("uri"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.trim_start_matches("spotify:artist:").to_string())
                                .unwrap_or_default();
                            if !name.is_empty() {
                                names.push(name.clone());
                                pairs.push((name, id));
                            }
                        }
                        (names.join(", "), pairs)
                    } else {
                        ("Unknown Artist".to_string(), Vec::new())
                    };

                    page_tracks.push(SpotifyTrackItem {
                        title,
                        artist: artist_names,
                        album,
                        duration_ms,
                        uri,
                        artists: artists_vec,
                        album_id,
                        uid, // <--- Add field to struct constructor
                    });
                }
            }

            if first_batch {
                first_batch = false;
                let _ = resp_tx.send(SpotifyResponse::PlaylistInitial {
                    playlist_id: playlist_id.to_string(),
                    tracks: page_tracks,
                    total,
                }).await;
            } else if !page_tracks.is_empty() {
                let _ = resp_tx.send(SpotifyResponse::PlaylistChunk {
                    playlist_id: playlist_id.to_string(),
                    tracks: page_tracks,
                }).await;
            }

            let has_next = val.get("next").map(|n| !n.is_null()).unwrap_or(false);
            if !has_next || page_count < limit || offset >= 3000 {
                break;
            }

            offset += page_count;
        }

        Ok(())
    }

    /// Scrapes public embed tokens and unrolls tracks using Spotify's exact Pathfinder v2 GraphQL query
    async fn fetch_public_embed_tracks(
        playlist_id: &str,
        resp_tx: &tokio::sync::mpsc::Sender<SpotifyResponse>,
    ) -> Result<bool> {
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .build()?;

        let mut access_token = String::new();
        let mut client_token = String::new();
        let mut _raw_embed_html = String::new();

        let embed_url = format!("https://open.spotify.com/embed/playlist/{}", playlist_id);
        if let Ok(resp) = client.get(&embed_url).send().await {
            if let Ok(text) = resp.text().await {
                _raw_embed_html = text.clone();
                let token_marker = r#""accessToken":""#;
                if let Some(start) = text.find(token_marker) {
                    let start_idx = start + token_marker.len();
                    if let Some(end_idx) = text[start_idx..].find('"') {
                        access_token = text[start_idx..start_idx + end_idx].to_string();
                    }
                }
            }
        }

        let ct_payload = serde_json::json!({
            "client_data": {
                "client_version": "1.2.40.584.g90c6665a",
                "client_id": "d8a5dc950d20472e8470d8641113735f",
                "js_sdk_data": { "device_brand": "unknown", "device_model": "desktop", "os": "Windows", "os_version": "NT 10.0" }
            }
        });

        if let Ok(resp) = client.post("https://clienttoken.spotify.com/v1/clienttoken").json(&ct_payload).send().await {
            if let Ok(ct_json) = resp.json::<Value>().await {
                if let Some(tok) = ct_json.pointer("/granted_token/token").and_then(|v| v.as_str()) {
                    client_token = tok.to_string();
                }
            }
        }

        let mut streamed_any = false;
        if !access_token.is_empty() {
            let mut offset = 0;
            let limit = 100;
            let operation_hash = "86dde7b9d9356e2369414647cf6950cfed96e778e129cfdfc99aea6c1613b3b0";
            let gql_url = "https://api-partner.spotify.com/pathfinder/v2/query";
            let mut first_batch = true;

            loop {
                let uri = format!("spotify:playlist:{}", playlist_id);
                let body = serde_json::json!({
                    "operationName": "fetchPlaylist",
                    "variables": { "uri": uri, "offset": offset, "limit": limit, "enableWatchFeedEntrypoint": false, "includeEpisodeContentRatingsV2": true },
                    "extensions": { "persistedQuery": { "version": 1, "sha256Hash": operation_hash } }
                });

                let mut req = client.post(gql_url)
                    .bearer_auth(&access_token)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json")
                    .header("app-platform", "WebPlayer")
                    .header("spotify-app-version", "1.2.40.584.g90c6665a")
                    .header("Origin", "https://open.spotify.com")
                    .header("Referer", "https://open.spotify.com/")
                    .json(&body);

                if !client_token.is_empty() {
                    req = req.header("client-token", &client_token);
                }

                let resp = match req.send().await {
                    Ok(r) => r,
                    Err(_) => break,
                };

                if !resp.status().is_success() { break; }

                let val: Value = match resp.json().await {
                    Ok(v) => v,
                    Err(_) => break,
                };

                let items_opt = val.pointer("/data/playlistV2/content/items")
                    .or_else(|| val.pointer("/data/playlist/content/items"))
                    .or_else(|| val.pointer("/data/items"))
                    .and_then(|v| v.as_array());

                let items = match items_opt {
                    Some(arr) if !arr.is_empty() => arr,
                    _ => break,
                };

                let total_count = val
                    .pointer("/data/playlistV2/content/totalCount")
                    .or_else(|| val.pointer("/data/playlist/content/totalCount"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;

                let page_count = items.len();
                let mut page_tracks = Vec::new();

                for item in items {
                    let item_data = match item.pointer("/itemV2/data").or_else(|| item.get("data")) {
                        Some(d) => d,
                        None => continue,
                    };
                    let title = item_data.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if title.is_empty() { continue; }
                    let uri = item_data.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    let duration_ms = item_data.pointer("/trackDuration/totalMilliseconds").or_else(|| item_data.get("duration_ms")).and_then(|v| v.as_u64()).unwrap_or(0);
                    let album = item_data.pointer("/albumOfTrack/name").or_else(|| item_data.pointer("/album/name")).and_then(|v| v.as_str()).unwrap_or("Playlist Track").to_string();

                    let album_id = item_data.pointer("/albumOfTrack/uri")
                        .or_else(|| item_data.pointer("/album/uri"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim_start_matches("spotify:album:").to_string());

                    // ---> INSERT HERE: Extract uid <---
                    let uid = item.get("uid")
                        .or_else(|| item_data.get("uid"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let (artist_names, artists_vec) = if let Some(arr) = item_data.pointer("/artists/items").or_else(|| item_data.get("artists")).and_then(|v| v.as_array()) {
                        let mut names = Vec::new();
                        let mut pairs = Vec::new();
                        for a in arr {
                            let name = a.pointer("/profile/name").or_else(|| a.get("name")).and_then(|n| n.as_str()).unwrap_or("").to_string();
                            let id = a.pointer("/profile/uri")
                                .or_else(|| a.get("uri"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.trim_start_matches("spotify:artist:").to_string())
                                .unwrap_or_default();
                            if !name.is_empty() {
                                names.push(name.clone());
                                pairs.push((name, id));
                            }
                        }
                        (names.join(", "), pairs)
                    } else {
                        ("Unknown Artist".to_string(), Vec::new())
                    };

                    page_tracks.push(SpotifyTrackItem {
                        title,
                        artist: artist_names,
                        album,
                        duration_ms,
                        uri,
                        artists: artists_vec,
                        album_id,
                        uid, // <--- Add field to struct constructor
                    });
                }

                if first_batch {
                    first_batch = false;
                    let _ = resp_tx.send(SpotifyResponse::PlaylistInitial {
                        playlist_id: playlist_id.to_string(),
                        tracks: page_tracks,
                        total: total_count,
                    }).await;
                } else if !page_tracks.is_empty() {
                    let _ = resp_tx.send(SpotifyResponse::PlaylistChunk {
                        playlist_id: playlist_id.to_string(),
                        tracks: page_tracks,
                    }).await;
                }

                streamed_any = true;
                offset += page_count;
                if page_count < limit || (total_count > 0 && offset >= total_count) || offset >= 3000 {
                    break;
                }
            }
        }

        Ok(streamed_any)
    }

    /// Fetches user saved tracks (Liked Songs) with pagination
    pub async fn get_liked_songs(&self) -> Result<Vec<SpotifyTrackItem>> {
        log::info!("Fetching Liked Songs...");
        self.ensure_token().await?; // <--- Refresh token if expired

        let mut tracks = Vec::new();
        let mut offset = 0;
        let limit = 50;

        loop {
            let endpoint = format!("me/tracks?limit={}&offset={}", limit, offset);
            let raw_json: String = match self.client.api_get(&endpoint, &Default::default()).await {
                Ok(j) => j,
                Err(e) => {
                    log::error!("Liked songs API error at offset {}: {:?}", offset, e);
                    break;
                }
            };

            let val: Value = match serde_json::from_str(&raw_json) {
                Ok(v) => v,
                Err(e) => {
                    log::error!("Failed to parse me/tracks JSON: {:?}", e);
                    break;
                }
            };

            let items = match val.get("items").and_then(|v| v.as_array()) {
                Some(arr) if !arr.is_empty() => arr,
                _ => break,
            };

            let page_count = items.len();

            for item in items {
                if let Some(t) = item.get("track") {
                    if t.is_null() { continue; }
                    let title = t.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown Track").to_string();
                    let album = t.pointer("/album/name").and_then(|v| v.as_str()).unwrap_or("Unknown Album").to_string();
                    let uri = t.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    let duration_ms = t.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);

                    let album_id = t.pointer("/album/id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let (artist_names, artists_vec) = if let Some(arr) = t.get("artists").and_then(|v| v.as_array()) {
                        let mut names = Vec::new();
                        let mut pairs = Vec::new();
                        for a in arr {
                            let name = a.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            let id = a.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                            if !name.is_empty() {
                                names.push(name.clone());
                                pairs.push((name, id));
                            }
                        }
                        (names.join(", "), pairs)
                    } else {
                        ("Unknown Artist".to_string(), Vec::new())
                    };

                    tracks.push(SpotifyTrackItem {
                        title,
                        artist: artist_names,
                        album,
                        duration_ms,
                        uri,
                        artists: artists_vec,
                        album_id,
                        uid: None,
                    });
                }
            }

            let has_next = val.get("next").map(|n| !n.is_null()).unwrap_or(false);
            if !has_next || page_count < limit {
                break;
            }

            offset += page_count;
            if offset >= 1000 {
                break;
            }
        }

        log::info!("Successfully parsed {} total liked tracks", tracks.len());
        Ok(tracks)
    }
    /// Fetches all tracks for an album using rspotify's auto-refreshing client
    pub async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<SpotifyTrackItem>> {
        log::info!("Fetching album tracks for ID: {}", album_id);

        let mut tracks = Vec::new();
        let mut offset = 0;
        let limit = 50;

        loop {
            let endpoint = format!("albums/{}/tracks?limit={}&offset={}", album_id, limit, offset);

            let raw_json: String = match self.client.api_get(&endpoint, &Default::default()).await {
                Ok(j) => j,
                Err(e) => {
                    log::warn!("Album tracks API request failed at offset {}: {:?}", offset, e);
                    break;
                }
            };

            let val: serde_json::Value = serde_json::from_str(&raw_json).unwrap_or_default();
            let items = match val.get("items").and_then(|v| v.as_array()) {
                Some(arr) if !arr.is_empty() => arr,
                _ => break,
            };

            let page_count = items.len();

            for track_obj in items {
                let title = track_obj.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if title.is_empty() {
                    continue;
                }

                let uri = track_obj.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let duration_ms = track_obj.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);

                let (artist_names, artists_vec) = if let Some(arr) = track_obj.get("artists").and_then(|v| v.as_array()) {
                    let mut names = Vec::new();
                    let mut pairs = Vec::new();
                    for a in arr {
                        let name = a.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                        if !name.is_empty() {
                            names.push(name.clone());
                            pairs.push((name, id));
                        }
                    }
                    (names.join(", "), pairs)
                } else {
                    ("Unknown Artist".to_string(), Vec::new())
                };

                tracks.push(SpotifyTrackItem {
                    title,
                    artist: artist_names,
                    album: "Album".to_string(),
                    duration_ms,
                    uri,
                    artists: artists_vec,
                    album_id: Some(album_id.trim_start_matches("spotify:album:").to_string()),
                    uid: None,
                });
            }

            let has_next = val.get("next").map(|n| !n.is_null()).unwrap_or(false);
            if !has_next || page_count < limit {
                break;
            }

            offset += page_count;
        }

        log::info!("Fetched {} tracks for album {}", tracks.len(), album_id);
        Ok(tracks)
    }

    /// Fetches an artist's top tracks via Pathfinder v2 queryArtistOverview
    pub async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<SpotifyTrackItem>> {
        log::info!("Fetching top tracks for artist ID via Pathfinder: {}", artist_id);

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .build()?;

        let mut access_token = String::new();
        let mut client_token = String::new();

        // 1. Session Token
        if let Ok(resp) = client.get("https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M").send().await {
            if let Ok(text) = resp.text().await {
                let marker = r#""accessToken":""#;
                if let Some(start) = text.find(marker) {
                    let start_idx = start + marker.len();
                    if let Some(end_idx) = text[start_idx..].find('"') {
                        access_token = text[start_idx..start_idx + end_idx].to_string();
                    }
                }
            }
        }

        // 2. Client Token
        let ct_payload = serde_json::json!({
            "client_data": {
                "client_version": "1.2.40.584.g90c6665a",
                "client_id": "d8a5dc950d20472e8470d8641113735f",
                "js_sdk_data": {
                    "device_brand": "unknown",
                    "device_model": "desktop",
                    "os": "Windows",
                    "os_version": "NT 10.0"
                }
            }
        });

        if let Ok(resp) = client
            .post("https://clienttoken.spotify.com/v1/clienttoken")
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("Origin", "https://open.spotify.com")
            .header("Referer", "https://open.spotify.com/")
            .json(&ct_payload)
            .send()
            .await
        {
            if let Ok(ct_json) = resp.json::<Value>().await {
                if let Some(tok) = ct_json.pointer("/granted_token/token").and_then(|v| v.as_str()) {
                    client_token = tok.to_string();
                }
            }
        }

        if access_token.is_empty() {
            anyhow::bail!("Failed to acquire active session token for artist overview");
        }

        // 3. Dispatch Pathfinder queryArtistOverview
        let operation_hash = "9f8134ef565e78621f1e1793555bd6633c5ac144ae0f89604ed3ae3f80b3c8e6";
        let gql_url = "https://api-partner.spotify.com/pathfinder/v2/query";
        let artist_uri = if artist_id.starts_with("spotify:artist:") {
            artist_id.to_string()
        } else {
            format!("spotify:artist:{}", artist_id)
        };

        let body = serde_json::json!({
            "operationName": "queryArtistOverview",
            "variables": {
                "uri": artist_uri,
                "locale": "",
                "preReleaseV2": false
            },
            "extensions": {
                "persistedQuery": {
                    "version": 1,
                    "sha256Hash": operation_hash
                }
            }
        });

        let mut req = client
            .post(gql_url)
            .bearer_auth(&access_token)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("app-platform", "WebPlayer")
            .header("spotify-app-version", "1.2.40.584.g90c6665a")
            .header("Origin", "https://open.spotify.com")
            .header("Referer", "https://open.spotify.com/")
            .json(&body);

        if !client_token.is_empty() {
            req = req.header("client-token", &client_token);
        }

        let resp = req.send().await.context("Artist overview request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let err_body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Pathfinder artist query returned HTTP {}: {}", status, err_body);
        }

        let val: Value = resp.json().await.context("Failed to parse artist overview response JSON")?;
        let mut tracks = Vec::new();

        if let Some(items) = val.pointer("/data/artistUnion/discography/topTracks/items").and_then(|v| v.as_array()) {
            for item in items {
                let track_data = match item.get("track") {
                    Some(t) => t,
                    None => continue,
                };

                let title = track_data.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if title.is_empty() {
                    continue;
                }

                let uri = track_data.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();

                let duration_ms = track_data
                    .pointer("/duration/totalMilliseconds")
                    .or_else(|| track_data.pointer("/trackDuration/totalMilliseconds"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let album = track_data
                    .pointer("/albumOfTrack/name")
                    .or_else(|| track_data.pointer("/album/name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Top Tracks")
                    .to_string();

                let artists = if let Some(arr) = track_data.pointer("/artists/items").and_then(|v| v.as_array()) {
                    arr.iter()
                        .filter_map(|a| a.pointer("/profile/name").or_else(|| a.get("name")).and_then(|n| n.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    "Unknown Artist".to_string()
                };

                let album_id = track_data
                    .pointer("/albumOfTrack/uri")
                    .or_else(|| track_data.pointer("/album/uri"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim_start_matches("spotify:album:").to_string());

                let clean_artist_id = artist_id.trim_start_matches("spotify:artist:").to_string();
                tracks.push(SpotifyTrackItem {
                    title,
                    artist: artists.clone(),
                    album,
                    duration_ms,
                    uri,
                    artists: vec![(artists, clean_artist_id)],
                    album_id,
                    uid: None,
                });
            }
        }

        log::info!("Artist overview returned {} top tracks", tracks.len());
        Ok(tracks)
    }
    /// Fetches full artist profile, discography, tour, and related content via Pathfinder v2
    pub async fn get_artist_overview(&self, artist_id: &str) -> Result<ArtistPageData> {
        log::info!("Querying complete artist page for ID: {}", artist_id);

        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .build()?;

        let mut access_token = String::new();
        let mut client_token = String::new();

        if let Ok(resp) = client.get("https://open.spotify.com/embed/playlist/37i9dQZF1DXcBWIGoYBM5M").send().await {
            if let Ok(text) = resp.text().await {
                let marker = r#""accessToken":""#;
                if let Some(start) = text.find(marker) {
                    let start_idx = start + marker.len();
                    if let Some(end_idx) = text[start_idx..].find('"') {
                        access_token = text[start_idx..start_idx + end_idx].to_string();
                    }
                }
            }
        }

        let ct_payload = serde_json::json!({
            "client_data": {
                "client_version": "1.2.40.584.g90c6665a",
                "client_id": "d8a5dc950d20472e8470d8641113735f",
                "js_sdk_data": { "device_brand": "unknown", "device_model": "desktop", "os": "Windows", "os_version": "NT 10.0" }
            }
        });

        if let Ok(resp) = client.post("https://clienttoken.spotify.com/v1/clienttoken").json(&ct_payload).send().await {
            if let Ok(ct_json) = resp.json::<Value>().await {
                if let Some(tok) = ct_json.pointer("/granted_token/token").and_then(|v| v.as_str()) {
                    client_token = tok.to_string();
                }
            }
        }

        let operation_hash = "9f8134ef565e78621f1e1793555bd6633c5ac144ae0f89604ed3ae3f80b3c8e6";
        let gql_url = "https://api-partner.spotify.com/pathfinder/v2/query";
        let artist_uri = if artist_id.starts_with("spotify:artist:") {
            artist_id.to_string()
        } else {
            format!("spotify:artist:{}", artist_id)
        };

        let body = serde_json::json!({
            "operationName": "queryArtistOverview",
            "variables": { "uri": artist_uri, "locale": "", "preReleaseV2": false },
            "extensions": { "persistedQuery": { "version": 1, "sha256Hash": operation_hash } }
        });

        let mut req = client.post(gql_url)
            .bearer_auth(&access_token)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("app-platform", "WebPlayer")
            .header("spotify-app-version", "1.2.40.584.g90c6665a")
            .header("Origin", "https://open.spotify.com")
            .header("Referer", "https://open.spotify.com/")
            .json(&body);

        if !client_token.is_empty() {
            req = req.header("client-token", &client_token);
        }

        let val: Value = req.send().await?.json().await?;
        let artist = val.pointer("/data/artistUnion").context("Artist payload missing")?;

        let mut page = ArtistPageData {
            id: artist_id.to_string(),
            name: artist.pointer("/profile/name").and_then(|v| v.as_str()).unwrap_or("Artist").to_string(),
            bio: artist.pointer("/profile/biography/text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            followers: artist.pointer("/stats/followers").and_then(|v| v.as_u64()).unwrap_or(0),
            monthly_listeners: artist.pointer("/stats/monthlyListeners").and_then(|v| v.as_u64()).unwrap_or(0),
            ..Default::default()
        };

        // Top Cities
        if let Some(cities) = artist.pointer("/stats/topCities/items").and_then(|v| v.as_array()) {
            for c in cities {
                let city = c.get("city").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let country = c.get("country").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let listeners = c.get("numberOfListeners").and_then(|v| v.as_u64()).unwrap_or(0);
                if !city.is_empty() {
                    page.top_cities.push((city, country, listeners));
                }
            }
        }

        // Top Tracks
        if let Some(items) = artist.pointer("/discography/topTracks/items").and_then(|v| v.as_array()) {
            for item in items {
                if let Some(t) = item.get("track") {
                    let title = t.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let uri = t.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    let duration_ms = t.pointer("/duration/totalMilliseconds").and_then(|v| v.as_u64()).unwrap_or(0);
                    let album = t.pointer("/albumOfTrack/name").and_then(|v| v.as_str()).unwrap_or("Unknown Album").to_string();
                    let artists = if let Some(arr) = t.pointer("/artists/items").and_then(|v| v.as_array()) {
                        arr.iter().filter_map(|a| a.pointer("/profile/name").and_then(|n| n.as_str())).collect::<Vec<_>>().join(", ")
                    } else { page.name.clone() };

                    let album_id = t.pointer("/albumOfTrack/uri")
                    .or_else(|| t.pointer("/album/uri"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim_start_matches("spotify:album:").to_string());

                    let clean_artist_id = artist_id.trim_start_matches("spotify:artist:").to_string();

                    page.top_tracks.push(SpotifyTrackItem {
                        title,
                        artist: artists.clone(),
                        album,
                        duration_ms,
                        uri,
                        artists: vec![(artists, clean_artist_id)],
                        album_id,
                        uid: None,
                    });
                }
            }
        }

        // Helper to extract release groups
        let extract_releases = |path: &str| -> Vec<ArtistReleaseItem> {
            let mut list = Vec::new();
            if let Some(groups) = artist.pointer(path).and_then(|v| v.as_array()) {
                for g in groups {
                    if let Some(rel) = g.pointer("/releases/items/0") {
                        let name = rel.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let uri = rel.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                        let release_type = rel.get("type").and_then(|v| v.as_str()).unwrap_or("ALBUM").to_string();
                        let year = rel.pointer("/date/year").and_then(|v| v.as_u64());
                        let track_count = rel.pointer("/tracks/totalCount").and_then(|v| v.as_u64()).unwrap_or(1);
                        if !name.is_empty() && !uri.is_empty() {
                            list.push(ArtistReleaseItem { name, uri, release_type, year, track_count });
                        }
                    }
                }
            }
            list
        };

        page.albums = extract_releases("/discography/albums/items");
        page.singles = extract_releases("/discography/singles/items");
        page.compilations = extract_releases("/discography/compilations/items");
        page.appears_on = extract_releases("/relatedContent/appearsOn/items");

        // Helper to extract playlist cards
        let extract_playlists = |path: &str| -> Vec<SpotifyPlaylistItem> {
            let mut list = Vec::new();
            if let Some(items) = artist.pointer(path).and_then(|v| v.as_array()) {
                for item in items {
                    if let Some(d) = item.get("data") {
                        if d.get("__typename").and_then(|v| v.as_str()) == Some("Playlist") {
                            let name = d.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let uri = d.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                            let id = d.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                            let owner_id = d.pointer("/ownerV2/data/username")
                                .or_else(|| d.pointer("/owner/id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("spotify")
                                .to_string();

                            if !name.is_empty() {
                                list.push(SpotifyPlaylistItem {
                                    name,
                                    id,
                                    tracks_total: 0,
                                    uri,
                                    owner_id, // <--- Fixed missing field
                                });
                            }
                        }
                    }
                }
            }
            list
        };

        page.artist_playlists = extract_playlists("/discography/playlistsV2/items");
        page.featuring_playlists = extract_playlists("/relatedContent/featuringV2/items");
        page.discovered_on = extract_playlists("/relatedContent/discoveredOnV2/items");

        // Related Artists (Fans Also Like)
        if let Some(items) = artist.pointer("/relatedContent/relatedArtists/items").and_then(|v| v.as_array()) {
            for a in items {
                let name = a.pointer("/profile/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let uri = a.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if !name.is_empty() {
                    page.related_artists.push((name, uri));
                }
            }
        }

        // On Tour (Concerts)
        if let Some(items) = artist.pointer("/goods/concerts/items").and_then(|v| v.as_array()) {
            for c in items {
                if let Some(d) = c.get("data") {
                    let title = d.get("title").and_then(|v| v.as_str()).unwrap_or("Concert").to_string();
                    let city = d.pointer("/location/city").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let venue = d.pointer("/location/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let date = d.get("startDateIsoString").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let uri = d.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    page.concerts.push(ArtistConcertItem { title, city, venue, date, uri });
                }
            }
        }

        Ok(page)
    }
    pub async fn start_playback(
        &self,
        target_uri: &str,
        context_uri: Option<&str>,
        upcoming_uris: Vec<String>,
    ) -> Result<()> {
        log::info!(
            "Triggering Web API playback: uri='{}', context={:?}, queued_window={}",
            target_uri,
            context_uri,
            upcoming_uris.len()
        );

        // 1. If it's a standard user playlist or album (not editorial 37i9dQZF), try context first
        let is_editorial = context_uri.map_or(false, |c| c.contains("37i9dQZF"));

        if let (Some(ctx), false) = (context_uri, is_editorial) {
            let context_id = if ctx.starts_with("spotify:playlist:") {
                PlaylistId::from_uri(ctx).ok().map(PlayContextId::Playlist)
            } else if ctx.starts_with("spotify:album:") {
                AlbumId::from_uri(ctx).ok().map(PlayContextId::Album)
            } else {
                None
            };

            if let Some(ctx_id) = context_id {
                let offset = if target_uri.starts_with("spotify:track:") {
                    Some(Offset::Uri(target_uri.to_string()))
                } else {
                    None
                };

                if let Ok(_) = self.client.start_context_playback(ctx_id, None, offset, None).await {
                    return Ok(());
                }
                log::warn!("Context playback failed; falling back to upcoming URI slice.");
            }
        }

        // 2. Playable Window Slicing: builds queue directly from displayed tracks
        let playables: Vec<PlayableId> = if !upcoming_uris.is_empty() {
            upcoming_uris
                .iter()
                .filter_map(|u| TrackId::from_uri(u).ok())
                .map(PlayableId::Track)
                .collect()
        } else if let Ok(track_id) = TrackId::from_uri(target_uri) {
            vec![PlayableId::Track(track_id)]
        } else {
            Vec::new()
        };

        if !playables.is_empty() {
            self.client
                .start_uris_playback(playables, None, None, None)
                .await
                .context("Failed to start URI window playback via Web API")?;
        }

        Ok(())
    }
    pub async fn get_current_user(&self) -> Result<(String, Option<String>, String)> {
        let raw_json: String = self.client.api_get("me", &Default::default()).await?;
        let val: Value = serde_json::from_str(&raw_json)?;
        let id = val.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let name = val.get("display_name").and_then(|v| v.as_str()).unwrap_or(&id).to_string();
        let avatar_url = val.pointer("/images/0/url").and_then(|v| v.as_str()).map(|s| s.to_string());
        Ok((name, avatar_url, id))
    }
    /// Fetches the user's saved albums
    pub async fn get_saved_albums(&self) -> Result<Vec<SpotifySavedAlbumItem>> {
        log::info!("Fetching user saved albums...");
        self.ensure_token().await?; // <--- Add this

        let raw_json: String = self
            .client
            .api_get("me/albums?limit=50&offset=0", &Default::default())
            .await
            .context("Failed to query me/albums")?;

        let val: serde_json::Value = serde_json::from_str(&raw_json)?;
        let mut albums = Vec::new();

        if let Some(items) = val.get("items").and_then(|v| v.as_array()) {
            for item in items {
                if let Some(album) = item.get("album") {
                    let name = album.get("name").and_then(|v| v.as_str()).unwrap_or("Untitled Album").to_string();
                    let id = album.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    let uri = album.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                    let artist = album.pointer("/artists/0/name").and_then(|v| v.as_str()).unwrap_or("Unknown Artist").to_string();

                    if !id.is_empty() {
                        albums.push(SpotifySavedAlbumItem { name, id, artist, uri });
                    }
                }
            }
        }
        Ok(albums)
    }

    /// Fetches the user's followed artists
    pub async fn get_followed_artists(&self) -> Result<Vec<SpotifyFollowedArtistItem>> {
        log::info!("Fetching user followed artists...");
        self.ensure_token().await?; // <--- Add this

        let raw_json: String = self
            .client
            .api_get("me/following?type=artist&limit=50", &Default::default())
            .await
            .context("Failed to query me/following")?;

        let val: serde_json::Value = serde_json::from_str(&raw_json)?;
        let mut artists = Vec::new();

        if let Some(items) = val.pointer("/artists/items").and_then(|v| v.as_array()) {
            for item in items {
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown Artist").to_string();
                let id = item.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let uri = item.get("uri").and_then(|v| v.as_str()).unwrap_or_default().to_string();

                if !id.is_empty() {
                    artists.push(SpotifyFollowedArtistItem { name, id, uri });
                }
            }
        }
        Ok(artists)
    }
    pub async fn ensure_token(&self) -> Result<()> {
        self.client
            .auto_reauth()
            .await
            .context("Failed to refresh Spotify access token")?;
        Ok(())
    }
    pub async fn check_tracks_saved(&self, track_ids: &[&str]) -> Result<Vec<bool>> {
        self.ensure_token().await?;
        let ids = track_ids.join(",");
        let raw = self.client.api_get(&format!("me/tracks/contains?ids={}", ids), &Default::default()).await?;
        let res: Vec<bool> = serde_json::from_str(&raw)?;
        Ok(res)
    }

    pub async fn set_track_saved(&self, track_id: &str, save: bool) -> Result<()> {
        self.ensure_token().await?;
        let endpoint = format!("me/tracks?ids={}", track_id);
        let empty_payload = serde_json::json!({});
        if save {
            self.client.api_put(&endpoint, &empty_payload).await?;
        } else {
            self.client.api_delete(&endpoint, &empty_payload).await?;
        }
        Ok(())
    }

    // --- ALBUMS ---

    pub async fn check_albums_saved(&self, album_ids: &[&str]) -> Result<Vec<bool>> {
        self.ensure_token().await?;
        let ids = album_ids.join(",");
        let raw = self.client.api_get(&format!("me/albums/contains?ids={}", ids), &Default::default()).await?;
        let res: Vec<bool> = serde_json::from_str(&raw)?;
        Ok(res)
    }

    pub async fn set_album_saved(&self, album_id: &str, save: bool) -> Result<()> {
        self.ensure_token().await?;
        let endpoint = format!("me/albums?ids={}", album_id);
        let empty_payload = serde_json::json!({});
        if save {
            self.client.api_put(&endpoint, &empty_payload).await?;
        } else {
            self.client.api_delete(&endpoint, &empty_payload).await?;
        }
        Ok(())
    }

    // --- ARTISTS ---

    pub async fn check_artists_followed(&self, artist_ids: &[&str]) -> Result<Vec<bool>> {
        self.ensure_token().await?;
        // Strip prefixes if present
        let clean_ids: Vec<&str> = artist_ids
            .iter()
            .map(|id| id.trim_start_matches("spotify:artist:"))
            .collect();
        let ids = clean_ids.join(",");
        let raw = self.client.api_get(&format!("me/following/contains?type=artist&ids={}", ids), &Default::default()).await?;
        let res: Vec<bool> = serde_json::from_str(&raw)?;
        Ok(res)
    }

    pub async fn set_artist_followed(&self, artist_id: &str, follow: bool) -> Result<()> {
        self.ensure_token().await?;
        let clean_id = artist_id.trim_start_matches("spotify:artist:");
        let endpoint = format!("me/following?type=artist&ids={}", clean_id);
        let empty_payload = serde_json::json!({});
        if follow {
            self.client.api_put(&endpoint, &empty_payload).await?;
        } else {
            self.client.api_delete(&endpoint, &empty_payload).await?;
        }
        Ok(())
    }

    pub async fn check_playlist_followed(&self, playlist_id: &str, user_id: &str) -> Result<bool> {
        self.ensure_token().await?;
        let clean_pl = playlist_id.trim_start_matches("spotify:playlist:");
        let clean_user = user_id.trim_start_matches("spotify:user:");
        let raw = self.client.api_get(
            &format!("playlists/{}/followers/contains?ids={}", clean_pl, clean_user),
            &Default::default(),
        ).await?;
        let res: Vec<bool> = serde_json::from_str(&raw)?;
        Ok(res.first().copied().unwrap_or(false))
    }

    pub async fn set_playlist_followed(&self, playlist_id: &str, follow: bool) -> Result<()> {
        self.ensure_token().await?;
        let clean_id = playlist_id.trim_start_matches("spotify:playlist:");
        let endpoint = format!("playlists/{}/followers", clean_id);
        if follow {
            let payload = serde_json::json!({ "public": false });
            self.client.api_put(&endpoint, &payload).await?;
        } else {
            let empty_payload = serde_json::json!({});
            self.client.api_delete(&endpoint, &empty_payload).await?;
        }
        Ok(())
    }
    /// Checks if any Spotify URIs are saved: GET /me/library/contains?uris=...
    pub async fn check_library_contains(&self, uris: &[&str]) -> Result<Vec<bool>> {
        self.ensure_token().await?;
        if uris.is_empty() {
            return Ok(Vec::new());
        }

        let query = uris.join(",");
        let endpoint = format!("me/library/contains?uris={}", query);

        let raw = match self.client.api_get(&endpoint, &Default::default()).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("me/library/contains query failed: {:?}", e);
                return Err(e.into());
            }
        };

        let res: Vec<bool> = serde_json::from_str(&raw)
            .context("Failed to parse me/library/contains JSON response")?;
        Ok(res)
    }

    pub async fn add_track_to_playlist(&self, playlist_id: &str, track_uri: &str) -> Result<()> {
        self.ensure_token().await?;
        let clean_id = playlist_id.trim_start_matches("spotify:playlist:");

        // Spotify's updated endpoint: POST /playlists/{playlist_id}/items
        let endpoint = format!("playlists/{}/items", clean_id);

        // Body with "uris" array and no "position" parameter so it appends
        let payload = serde_json::json!({
            "uris": [track_uri]
        });

        // Attempt updated /items endpoint; fallback to /tracks if rejected
        if let Err(_) = self.client.api_post(&endpoint, &payload).await {
            let legacy_endpoint = format!("playlists/{}/tracks", clean_id);
            self.client.api_post(&legacy_endpoint, &payload).await?;
        }

        Ok(())
    }

    pub async fn remove_track_from_playlist(
        &self,
        playlist_id: &str,
        updated_uris: &[String],
    ) -> Result<()> {
        self.ensure_token().await?;
        let clean_id = playlist_id.trim_start_matches("spotify:playlist:");

        // First 100 tracks to replace playlist contents via PUT
        let first_batch: Vec<&str> = updated_uris.iter().take(100).map(|s| s.as_str()).collect();
        let put_endpoint = format!("playlists/{}/items", clean_id);
        let put_payload = serde_json::json!({
            "uris": first_batch
        });

        // Try updated /items endpoint; fallback to /tracks if needed
        if let Err(_) = self.client.api_put(&put_endpoint, &put_payload).await {
            let legacy_put = format!("playlists/{}/tracks", clean_id);
            self.client.api_put(&legacy_put, &put_payload).await?;
        }

        // If the playlist has more than 100 tracks, append the remaining ones
        if updated_uris.len() > 100 {
            for chunk in updated_uris[100..].chunks(100) {
                let post_endpoint = format!("playlists/{}/items", clean_id);
                let post_payload = serde_json::json!({
                    "uris": chunk
                });

                if let Err(_) = self.client.api_post(&post_endpoint, &post_payload).await {
                    let legacy_post = format!("playlists/{}/tracks", clean_id);
                    self.client.api_post(&legacy_post, &post_payload).await?;
                }
            }
        }

        log::info!("Successfully updated playlist without duplicate collision.");
        Ok(())
    }

    /// Adds items to the user's library using PUT /me/library?uris=...
    pub async fn add_to_library(&self, uris: &[&str]) -> Result<()> {
        self.ensure_token().await?;
        if uris.is_empty() {
            return Ok(());
        }

        let encoded_uris = uris.join(",");
        let endpoint = format!("me/library?uris={}", encoded_uris);
        let empty_payload = serde_json::json!({});

        self.client.api_put(&endpoint, &empty_payload).await?;
        Ok(())
    }

    /// Removes items from the user's library using DELETE /me/library?uris=...
    pub async fn remove_from_library(&self, uris: &[&str]) -> Result<()> {
        self.ensure_token().await?;
        if uris.is_empty() {
            return Ok(());
        }

        let encoded_uris = uris.join(",");
        let endpoint = format!("me/library?uris={}", encoded_uris);
        let empty_payload = serde_json::json!({});

        self.client.api_delete(&endpoint, &empty_payload).await?;
        Ok(())
    }

    pub async fn add_to_queue(&self, track_uri: &str) -> Result<()> {
        self.ensure_token().await?;
        let endpoint = format!("me/player/queue?uri={}", track_uri);
        let empty_payload = serde_json::json!({});
        self.client.api_post(&endpoint, &empty_payload).await?;
        Ok(())
    }

    /// Fetches all active & available Spotify Connect devices
    pub async fn get_available_devices(&self) -> Result<Vec<SpotifyDeviceItem>> {
        self.ensure_token().await?;
        let raw_json: String = self.client.api_get("me/player/devices", &Default::default()).await?;
        let val: serde_json::Value = serde_json::from_str(&raw_json)?;

        let mut devices = Vec::new();
        if let Some(items) = val.get("devices").and_then(|v| v.as_array()) {
            for d in items {
                let id = d.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = d.get("name").and_then(|v| v.as_str()).unwrap_or("Unknown Device").to_string();
                let device_type = d.get("type").and_then(|v| v.as_str()).unwrap_or("Speaker").to_string();
                let is_active = d.get("is_active").and_then(|v| v.as_bool()).unwrap_or(false);
                let volume_percent = d.get("volume_percent").and_then(|v| v.as_u64()).map(|v| v as u32);

                if !id.is_empty() {
                    devices.push(SpotifyDeviceItem {
                        id,
                        name,
                        device_type,
                        is_active,
                        volume_percent,
                    });
                }
            }
        }
        Ok(devices)
    }

    /// Transfers playback to target device ID
    pub async fn transfer_playback(&self, device_id: &str, play: bool) -> Result<()> {
        self.ensure_token().await?;
        let endpoint = "me/player";
        let payload = serde_json::json!({
            "device_ids": [device_id],
            "play": play
        });
        self.client.api_put(endpoint, &payload).await?;
        Ok(())
    }
}