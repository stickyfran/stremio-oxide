use crate::{
    MainWindow, NavigationController, NavigationIntent,
    app_model::{AppModel, AppModelField},
    config::AppConfig,
    models,
    mpv_integration::NativePlaybackBridge,
    playback::PlaybackSelections,
};
use core_env::DesktopEnv;
use slint::{ComponentHandle, Model};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use stremio_core::{
    models::player::Selected,
    models::{
        installed_addons_with_filters::InstalledAddonsRequest,
        library_with_filters::LibraryRequest, library_with_filters::Sort,
    },
    runtime::{
        Runtime, RuntimeAction,
        msg::{Action, ActionCatalogsWithExtra, ActionLoad},
    },
};

static PENDING_TORRENT_FALLBACK: std::sync::LazyLock<std::sync::Mutex<Option<Selected>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

pub fn clear_pending_torrent_fallback() {
    *PENDING_TORRENT_FALLBACK.lock().unwrap() = None;
}

pub fn setup_ui_callbacks(
    ui: &MainWindow,
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    playback_selections: &Arc<PlaybackSelections>,
    native_playback_bridge: &Option<NativePlaybackBridge>,
    config: &AppConfig,
    navigation: NavigationController,
    download_manager: Arc<crate::downloads::DownloadManager>,
    server_readiness: crate::server_readiness::ServerReadiness,
) {
    let ui_weak = ui.as_weak();
    // Hook up submodel setup functions
    models::auth::setup(ui, runtime);
    models::board::setup(ui, runtime, &navigation);
    models::calendar::setup(ui, runtime, &navigation);
    models::discover::setup(ui, runtime, &navigation);
    models::events::setup(ui, runtime);
    models::library::setup(ui, runtime, &navigation);
    models::search::setup(ui, runtime, &navigation);
    models::addons::setup(ui, runtime, &navigation);
    crate::community_addons::setup(ui);
    models::details::setup(ui, runtime, &navigation);
    models::settings::setup(ui, runtime, config, native_playback_bridge.as_ref());
    models::onboarding::setup(ui, config);

    ui.on_details_ranking_mode_changed({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let ui_weak = ui_weak.clone();
        move |mode| {
            let mode = match mode.as_str() {
                "Quality" => stream_ranking::RankingMode::Quality,
                "Smallest" => stream_ranking::RankingMode::Smallest,
                "Seeders" => stream_ranking::RankingMode::Seeders,
                "Original" => stream_ranking::RankingMode::Original,
                _ => stream_ranking::RankingMode::Smart,
            };
            playback_selections.set_ranking_mode(mode);
            project_current_streams(&runtime, &playback_selections, &ui_weak);
            persist_profile_setting("stream-ranking-mode", ranking_mode_name(mode));
        }
    });

    ui.on_details_show_filtered_changed({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let ui_weak = ui_weak.clone();
        move |show| {
            playback_selections.set_show_filtered(show);
            project_current_streams(&runtime, &playback_selections, &ui_weak);
            persist_profile_setting("stream-show-filtered", if show { "true" } else { "false" });
        }
    });

    // Play stream action
    let server_readiness_play = server_readiness.clone();
    ui.on_play_stream({
        let ui_weak = ui_weak.clone();
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let native_playback_bridge = native_playback_bridge.clone();
        let navigation = navigation.clone();
        let server_readiness = server_readiness_play.clone();
        move |selection_id| {
            let switching_in_player = navigation.is_player_visible();
            tracing::info!(
                selection_id = %selection_id,
                native_playback_available = native_playback_bridge.is_some(),
                "playback selection requested"
            );
            let Some((selected, stream_name)) = playback_selections.resolve(selection_id.as_str())
            else {
                tracing::warn!(selection_id = %selection_id, "playback selection expired");
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_error_message(
                        "That stream is no longer available. Choose it again.".into(),
                    );
                }
                return;
            };

            if switching_in_player && let Some(bridge) = native_playback_bridge.as_ref() {
                bridge.prepare_stream_switch();
            }

            if let Some(ui) = ui_weak.upgrade() {
                let detail_title = ui.get_detail_title().to_string();
                let is_series = ui.get_detail_is_series();
                let player_title = if is_series {
                    let active_ep = ui.get_detail_active_episode_idx();
                    let episodes = ui.get_detail_episodes();
                    if let Some(ep) = episodes.iter().nth(active_ep as usize) {
                        let ep_title = ep.title.to_string();
                        let season = ui.get_detail_active_season();
                        let ep_num = ep.episode_num;
                        if !ep_title.is_empty() && ep_title != detail_title {
                            if ep_num > 0 && season > 0 {
                                format!("{detail_title} - {ep_title} ({season}x{ep_num})")
                            } else {
                                format!("{detail_title} - {ep_title}")
                            }
                        } else if ep_num > 0 && season > 0 {
                            format!("{detail_title} ({season}x{ep_num})")
                        } else {
                            detail_title.clone()
                        }
                    } else {
                        detail_title.clone()
                    }
                } else {
                    detail_title.clone()
                };

                ui.set_player_title(player_title.into());
                ui.set_player_stream_name(stream_name.clone().into());
                ui.set_player_poster_image(ui.get_detail_poster());
                ui.set_player_video_frame(slint::Image::default());
                ui.set_player_has_video_frame(false);
                ui.set_player_error("".into());
                ui.set_player_loading(true);
                ui.set_player_debrid_uncached(false);
                clear_pending_torrent_fallback();
                ui.set_player_loading_stalled(false);
                ui.set_player_loading_stage_title("Iniciando reproducción…".into());
                ui.set_player_loading_stage_detail("Resolviendo enlace del stream…".into());
                ui.set_player_buffering(false);
                ui.set_player_buffering_percent(0.0);
                ui.set_player_controls_visible(true);
                ui.set_player_is_series(is_series);
                if is_series {
                    ui.set_player_seasons(ui.get_detail_seasons());
                    ui.set_player_active_season(ui.get_detail_active_season());
                    ui.set_player_episodes(ui.get_detail_episodes());
                    ui.set_player_active_episode_idx(ui.get_detail_active_episode_idx());
                    ui.set_player_active_video_id(
                        selected
                            .stream_request
                            .as_ref()
                            .map(|request| request.path.id.as_str())
                            .unwrap_or_default()
                            .into(),
                    );
                } else {
                    ui.set_player_seasons(Default::default());
                    ui.set_player_episodes(Default::default());
                    ui.set_player_active_video_id("".into());
                    ui.set_player_active_episode_idx(0);
                }
                ui.set_player_has_next_episode(false);
                ui.invoke_close_player_menus();
                ui.invoke_focus_app_shortcuts();
                navigation.dispatch_and_project(&ui, NavigationIntent::OpenPlayer);
            }

            let ui_weak = ui_weak.clone();
            let runtime = runtime.clone();
            let server_readiness = server_readiness.clone();
            tokio::spawn(async move {
                // Debrid unrestriction only happens after this explicit click.
                // Any provider failure leaves the original source untouched.
                let resolution = crate::integrations::resolve_explicit_selection(selected).await;
                match resolution {
                    crate::integrations::DebridResolution::Resolved(selected) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_player_debrid_uncached(false);
                                ui.set_player_loading_stage_title("Conectando al servidor Debrid…".into());
                                ui.set_player_loading_stage_detail("Obteniendo stream optimizado de alta velocidad…".into());
                            }
                            runtime.dispatch(RuntimeAction {
                                field: None,
                                action: Action::Load(ActionLoad::Player(Box::new(selected))),
                            });
                        });
                    }
                    crate::integrations::DebridResolution::Uncached { provider_name, selected } => {
                        let _ = slint::invoke_from_event_loop(move || {
                            *PENDING_TORRENT_FALLBACK.lock().unwrap() = Some(selected);
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_player_debrid_uncached(true);
                                ui.set_player_loading_stage_title(
                                    format!("No disponible en caché de {provider_name}").into(),
                                );
                                ui.set_player_loading_stage_detail(
                                    "Este stream no está en la nube de Debrid. Puedes reproducirlo vía Torrent P2P o volver y elegir otro stream.".into(),
                                );
                            }
                        });
                    }
                    crate::integrations::DebridResolution::Fallback(selected) => {
                        let is_torrent = matches!(
                            &selected.stream.source,
                            stremio_core::types::resource::StreamSource::Torrent { .. }
                        );
                        if is_torrent && !server_readiness.is_ready() {
                            let _ = slint::invoke_from_event_loop({
                                let ui_weak = ui_weak.clone();
                                move || {
                                    if let Some(ui) = ui_weak.upgrade() {
                                        ui.set_player_loading_stage_title("Iniciando motor de streaming…".into());
                                        ui.set_player_loading_stage_detail("Esperando conexión local de libtorrent…".into());
                                    }
                                }
                            });
                            if !server_readiness.wait_ready(std::time::Duration::from_secs(6)).await {
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak.upgrade() {
                                        ui.set_player_loading_stage_title("Servidor de streaming no disponible".into());
                                        ui.set_player_loading_stage_detail(
                                            "El motor local tardó demasiado en iniciar. Reintenta la reproducción o revisa Ajustes.".into(),
                                        );
                                        ui.set_player_error(
                                            "No se pudo conectar con el motor de streaming local.".into(),
                                        );
                                    }
                                });
                                return;
                            }
                        }
                        let (stage_title, stage_detail) = if is_torrent {
                            (
                                "Conectando con enjambre…",
                                "Buscando pares e intercambiando metadatos…",
                            )
                        } else {
                            (
                                "Conectando al servidor…",
                                "Preparando buffer del stream…",
                            )
                        };
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_player_debrid_uncached(false);
                                ui.set_player_loading_stage_title(stage_title.into());
                                ui.set_player_loading_stage_detail(stage_detail.into());
                            }
                            runtime.dispatch(RuntimeAction {
                                field: None,
                                action: Action::Load(ActionLoad::Player(Box::new(selected))),
                            });
                        });
                    }
                }
            });
        }
    });

    // Download action (single episode/movie, season, or entire series)
    let download_manager_clone = download_manager.clone();
    let runtime_for_download = runtime.clone();
    ui.on_start_download({
        let ui_weak = ui_weak.clone();
        let playback_selections = playback_selections.clone();
        let download_manager = download_manager_clone.clone();
        let runtime = runtime_for_download;
        move |selection_id, scope, target_season| {
            let Some((selected, _stream_name)) = playback_selections.resolve(selection_id.as_str())
            else {
                tracing::warn!(selection_id = %selection_id, "download selection expired");
                return;
            };

            let Some(ui) = ui_weak.upgrade() else {
                return;
            };

            let raw_detail_title = ui.get_detail_title().to_string();
            let is_series = ui.get_detail_is_series();
            let scope = scope.to_string();

            let model = runtime.model().expect("model read failed");
            let settings = model.ctx.profile.settings.clone();
            let streaming_server_url = model.streaming_server.base_url.clone();
            let installed_addons = model.ctx.profile.addons.clone();
            let user_sub_lang = settings.subtitles_language.clone();
            let meta_item = model.meta_details.meta_items.iter().find_map(|r| match &r.content {
                Some(stremio_core::models::common::Loadable::Ready(item)) => Some(item.clone()),
                _ => None,
            });
            let detail_title = if !raw_detail_title.is_empty() {
                raw_detail_title
            } else {
                meta_item.as_ref().map(|m| m.preview.name.clone()).unwrap_or_else(|| "Video".into())
            };
            let meta_id = meta_item.as_ref().map(|m| m.preview.id.clone()).or_else(|| {
                model.meta_details.selected.as_ref().map(|s| s.meta_path.id.clone())
            });
            let poster_url = meta_item.as_ref()
                .and_then(|m| m.preview.poster.as_ref().map(|u| u.to_string()))
                .or_else(|| {
                    let u = ui.get_detail_poster_url().to_string();
                    if !u.is_empty() { Some(u) } else { None }
                });
            let orig_lang: Option<String> = meta_item.as_ref().and_then(|meta| {
                let id_lower = meta.preview.id.to_ascii_lowercase();
                let type_lower = meta.preview.r#type.to_ascii_lowercase();
                let is_anime = id_lower.starts_with("kitsu:")
                    || type_lower == "anime"
                    || meta.preview.links.iter().any(|l| {
                        (l.category.eq_ignore_ascii_case("genre") || l.category.eq_ignore_ascii_case("genres"))
                            && (l.name.eq_ignore_ascii_case("anime") || l.name.eq_ignore_ascii_case("animation"))
                    });
                if is_anime {
                    Some("ja".to_string())
                } else {
                    Some("en".to_string())
                }
            });

            if is_series {
                if let (Some(meta_id_val), Some(meta)) = (&meta_id, &meta_item) {
                    let eps: Vec<crate::downloads::CachedEpisode> = meta.videos.iter().filter_map(|v| {
                        let info = v.series_info.as_ref()?;
                        Some(crate::downloads::CachedEpisode {
                            id: v.id.clone(),
                            title: v.title.clone(),
                            season: info.season as i32,
                            episode: info.episode as i32,
                            released: v.released.map(|dt| dt.format("%Y-%m-%d").to_string()),
                        })
                    }).collect();
                    if !eps.is_empty() {
                        let m_id = meta_id_val.clone();
                        tokio::spawn(async move {
                            if let Ok(json) = serde_json::to_string(&eps) {
                                let _ = crate::db::set_series_episodes_cache(&m_id, &json).await;
                            }
                        });
                    }
                }
            }

            drop(model);

            let current_season = ui.get_detail_active_season();
            let (current_ep_name, current_ep_num) = {
                let active_ep_idx = ui.get_detail_active_episode_idx() as usize;
                let episodes = ui.get_detail_episodes();
                if let Some(ep) = episodes.iter().nth(active_ep_idx) {
                    (ep.title.to_string(), ep.episode_num)
                } else {
                    (String::new(), 1)
                }
            };

            let base_downloads_dir = crate::paths::get().downloads().to_path_buf();
            let title_folder = base_downloads_dir.join(crate::downloads::sanitize_filename(&detail_title));

            let download_manager = download_manager.clone();
            let ui_weak_inner = ui_weak.clone();

            tokio::spawn(async move {
                let profile_id = crate::profiles::active_profile_id()
                    .await
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_else(|_| "default".to_string());
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .dns_resolver(core_env::shared_dns_resolver())
                    .build()
                    .unwrap_or_default();

                let server_base = streaming_server_url
                    .as_ref()
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| "http://127.0.0.1:11470/".into());
                let server_trimmed = server_base.trim_end_matches('/');

                let spawn_subtitles_for_job = |job: crate::downloads::DownloadJob,
                                                media_type: &'static str,
                                                vid_id: String,
                                                subs: Vec<stremio_core::types::resource::Subtitles>| {
                    let dm = download_manager.clone();
                    let cl = client.clone();
                    let prof = profile_id.clone();
                    let addons = installed_addons.clone();
                    let u_lang = user_sub_lang.clone();
                    let o_lang = orig_lang.clone();
                    let weak_sub = ui_weak_inner.clone();
                    tokio::spawn(async move {
                        crate::downloads::fetch_and_enqueue_subtitles(
                            dm,
                            &cl,
                            &prof,
                            &job,
                            media_type,
                            &vid_id,
                            &subs,
                            &addons,
                            u_lang.as_deref(),
                            o_lang.as_deref(),
                        )
                        .await;
                        crate::downloads::project_active_profile(weak_sub).await;
                    });
                };

                if (scope == "season" || scope == "series") && is_series {
                    let mut aired_episodes = meta_item
                        .as_ref()
                        .map(|m| crate::downloads::collect_aired_episodes(m, &scope, target_season))
                        .unwrap_or_default();
                    if aired_episodes.is_empty() {
                        let existing_for_cache = crate::downloads::list_jobs(&profile_id)
                            .await
                            .unwrap_or_default();
                        let cached = crate::downloads::get_series_all_episodes(
                            &client,
                            meta_id.as_deref().unwrap_or_default(),
                            &existing_for_cache,
                        )
                        .await;
                        aired_episodes = cached
                            .into_iter()
                            .filter(|ep| {
                                if scope == "season" {
                                    ep.season == target_season
                                } else {
                                    ep.season > 0
                                }
                            })
                            .map(|ep| crate::downloads::EpisodeTarget {
                                season: ep.season,
                                episode: ep.episode,
                                video_id: if ep.id.is_empty() {
                                    meta_id
                                        .as_deref()
                                        .map(|m| format!("{m}:{}:{}", ep.season, ep.episode))
                                        .unwrap_or_default()
                                } else {
                                    ep.id
                                },
                                title: ep.title,
                            })
                            .collect();
                    }
                    if aired_episodes.is_empty() {
                        aired_episodes.push(crate::downloads::EpisodeTarget {
                            season: current_season,
                            episode: current_ep_num,
                            video_id: meta_id
                                .as_deref()
                                .map(|m| format!("{m}:{current_season}:{current_ep_num}"))
                                .unwrap_or_default(),
                            title: current_ep_name.clone(),
                        });
                    }

                    let existing_jobs = crate::downloads::list_jobs(&profile_id)
                        .await
                        .unwrap_or_default();
                    let mut to_download = Vec::new();
                    let mut skipped_count = 0usize;

                    for ep in aired_episodes {
                        let already_active = existing_jobs.iter().any(|j| {
                            j.kind == "video"
                                && j.meta_id.as_deref() == meta_id.as_deref()
                                && j.season == Some(ep.season)
                                && j.episode == Some(ep.episode)
                                && matches!(
                                    j.state,
                                    crate::downloads::DownloadState::Completed
                                        | crate::downloads::DownloadState::Downloading
                                        | crate::downloads::DownloadState::Resolving
                                        | crate::downloads::DownloadState::Queued
                                )
                        });
                        if already_active {
                            skipped_count += 1;
                        } else {
                            crate::downloads::delete_stale_episode_jobs(
                                &download_manager,
                                &profile_id,
                                meta_id.as_deref(),
                                Some(ep.season),
                                Some(ep.episode),
                            )
                            .await;
                            to_download.push(ep);
                        }
                    }

                    if to_download.is_empty() {
                        tracing::info!(
                            skipped_count,
                            scope = %scope,
                            "all episodes in requested scope are already downloaded or active"
                        );
                        crate::downloads::project_active_profile(ui_weak_inner).await;
                        return;
                    }

                    // Tier 1: Torrent pack fast-path (single info_hash containing multiple episode files).
                    if let stremio_core::types::resource::StreamSource::Torrent {
                        info_hash, ..
                    } = &selected.stream.source
                    {
                        let info_hash_hex = hex::encode(info_hash);
                        let stats_url = format!("{server_trimmed}/{info_hash_hex}/stats.json");
                        let create_url = format!("{server_trimmed}/{info_hash_hex}/create");
                        let _ = client
                            .post(&create_url)
                            .json(&serde_json::json!({}))
                            .timeout(std::time::Duration::from_secs(3))
                            .send()
                            .await;

                        let mut file_list: Option<Vec<serde_json::Value>> = None;
                        for _ in 0..6 {
                            if let Ok(resp) = client
                                .get(&stats_url)
                                .timeout(std::time::Duration::from_secs(3))
                                .send()
                                .await
                                && resp.status().is_success()
                                && let Ok(json) = resp.json::<serde_json::Value>().await
                                && let Some(files) = json.get("files").and_then(|f| f.as_array())
                                && !files.is_empty()
                            {
                                file_list = Some(files.clone());
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }

                        if let Some(files) = file_list {
                            let mut matched_pairs = std::collections::HashSet::new();
                            for (file_idx, file) in files.iter().enumerate() {
                                let file_name = file
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .or_else(|| file.get("path").and_then(|p| p.as_str()))
                                    .unwrap_or("");
                                let lower_name = file_name.to_ascii_lowercase();
                                let is_video = lower_name.ends_with(".mkv")
                                    || lower_name.ends_with(".mp4")
                                    || lower_name.ends_with(".avi")
                                    || lower_name.ends_with(".ts")
                                    || lower_name.ends_with(".mov");
                                if !is_video {
                                    continue;
                                }
                                if let Some((s, e)) =
                                    crate::downloads::parse_season_episode_from_filename(file_name)
                                    && to_download
                                        .iter()
                                        .any(|ep| ep.season == s && ep.episode == e)
                                    && matched_pairs.insert((s, e))
                                {
                                    let file_stream_url =
                                        format!("{server_trimmed}/{info_hash_hex}/{file_idx}");
                                    let ep_dest_dir =
                                        title_folder.join(format!("Temporada {:02}", s));
                                    let ep_title = format!("{detail_title} S{:02}E{:02}", s, e);
                                    let ep_filename =
                                        format!("{detail_title} - S{:02}E{:02}.mp4", s, e);
                                    let video_id =
                                        meta_id.as_ref().map(|m| format!("{m}:{s}:{e}"));

                                    if let Ok(job) = download_manager
                                        .enqueue_with_options(
                                            &profile_id,
                                            &ep_title,
                                            &ep_filename,
                                            &ep_dest_dir,
                                            crate::downloads::DownloadSource {
                                                url: file_stream_url,
                                                headers: Vec::new(),
                                                kind: crate::downloads::DownloadSourceKind::DirectHttp,
                                            },
                                            crate::downloads::EnqueueOptions {
                                                meta_id: meta_id.clone(),
                                                meta_type: Some("series".into()),
                                                series_name: Some(detail_title.clone()),
                                                season: Some(s),
                                                episode: Some(e),
                                                video_id: video_id.clone(),
                                                poster_url: poster_url.clone(),
                                                kind: Some("video".into()),
                                                parent_job_id: None,
                                                lang: None,
                                            },
                                        )
                                        .await
                                    {
                                        spawn_subtitles_for_job(
                                            job,
                                            "series",
                                            video_id.unwrap_or_default(),
                                            selected.stream.subtitles.clone(),
                                        );
                                    }
                                }
                            }
                            if !matched_pairs.is_empty() {
                                to_download
                                    .retain(|ep| !matched_pairs.contains(&(ep.season, ep.episode)));
                                crate::downloads::project_active_profile(ui_weak_inner.clone())
                                    .await;
                            }
                        }
                    }

                    // Tier 2: Smart per-episode stream discovery & fallback resolution.
                    if !to_download.is_empty() {
                        let seed_name = selected.stream.name.clone().unwrap_or_default();
                        let seed_desc = selected.stream.description.clone().unwrap_or_default();
                        let seed_size = selected
                            .stream
                            .behavior_hints
                            .video_size
                            .filter(|&sz| sz > 0)
                            .or_else(|| stream_ranking::parse_size_bytes_from_text(&seed_desc))
                            .or_else(|| stream_ranking::parse_size_bytes_from_text(&seed_name));
                        let seed_input = stream_ranking::RankInput {
                            id: "selected".to_owned(),
                            name: seed_name,
                            description: seed_desc,
                            addon: selected
                                .stream_request
                                .as_ref()
                                .map(|r| r.base.to_string())
                                .unwrap_or_default(),
                            original_index: 0,
                            size_bytes: seed_size,
                            seeders: None,
                            debrid: None,
                        };
                        let profile = stream_ranking::StreamMatchProfile::from_rank_input(
                            &seed_input,
                            matches!(
                                selected.stream.source,
                                stremio_core::types::resource::StreamSource::Torrent { .. }
                            ),
                        );

                        let mut placeholders = Vec::with_capacity(to_download.len());
                        for ep in to_download {
                            let ep_dest_dir =
                                title_folder.join(format!("Temporada {:02}", ep.season));
                            let ep_title = if !ep.title.is_empty() && ep.title != detail_title {
                                format!("{detail_title} - {} ({}x{})", ep.title, ep.season, ep.episode)
                            } else {
                                format!("{detail_title} ({}x{})", ep.season, ep.episode)
                            };
                            let ep_filename = format!(
                                "{}.mp4",
                                crate::downloads::sanitize_filename(&ep_title)
                            );
                            if let Ok(job) = download_manager
                                .create_placeholder_job(
                                    &profile_id,
                                    &ep_title,
                                    &ep_filename,
                                    &ep_dest_dir,
                                    crate::downloads::DownloadState::Resolving,
                                    None,
                                    crate::downloads::EnqueueOptions {
                                        meta_id: meta_id.clone(),
                                        meta_type: Some("series".into()),
                                        series_name: Some(detail_title.clone()),
                                        season: Some(ep.season),
                                        episode: Some(ep.episode),
                                        video_id: Some(ep.video_id.clone()),
                                        poster_url: poster_url.clone(),
                                        kind: Some("video".into()),
                                        parent_job_id: None,
                                        lang: None,
                                    },
                                )
                                .await
                            {
                                placeholders.push((ep, job));
                            }
                        }
                        crate::downloads::project_active_profile(ui_weak_inner.clone()).await;

                        for (idx, (ep, job)) in placeholders.into_iter().enumerate() {
                            if idx > 0 {
                                tokio::time::sleep(std::time::Duration::from_millis(180)).await;
                            }

                            let mut resolved_entry: Option<(
                                String,
                                stremio_core::types::resource::Stream,
                                Option<u64>,
                            )> = None;

                            if ep.season == current_season && ep.episode == current_ep_num {
                                resolved_entry = crate::downloads::resolve_stream_to_download_url(
                                    selected.stream.clone(),
                                    selected.stream_request.clone(),
                                    selected.meta_request.clone(),
                                    &streaming_server_url,
                                    &settings,
                                    &client,
                                    false,
                                )
                                .await;
                            }

                            if resolved_entry.is_none() && !ep.video_id.is_empty() {
                                let candidates =
                                    crate::downloads::find_and_rank_episode_candidates(
                                        &profile,
                                        &installed_addons,
                                        &ep.video_id,
                                    )
                                    .await;
                                for (addon, cand_stream) in candidates.into_iter().take(3) {
                                    let req = stremio_core::types::addon::ResourceRequest {
                                        base: addon.transport_url.clone(),
                                        path: stremio_core::types::addon::ResourcePath::without_extra(
                                            "stream",
                                            "series",
                                            &ep.video_id,
                                        ),
                                    };
                                    if let Some(res) =
                                        crate::downloads::resolve_stream_to_download_url(
                                            cand_stream,
                                            Some(req),
                                            selected.meta_request.clone(),
                                            &streaming_server_url,
                                            &settings,
                                            &client,
                                            false,
                                        )
                                        .await
                                    {
                                        resolved_entry = Some(res);
                                        break;
                                    }
                                }
                            }

                            if let Some((url, final_stream, size_hint)) = resolved_entry {
                                match download_manager
                                    .activate_placeholder_job(
                                        &job,
                                        crate::downloads::DownloadSource {
                                            url,
                                            headers: Vec::new(),
                                            kind: crate::downloads::DownloadSourceKind::DirectHttp,
                                        },
                                        size_hint,
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        spawn_subtitles_for_job(
                                            job,
                                            "series",
                                            ep.video_id.clone(),
                                            final_stream.subtitles,
                                        );
                                    }
                                    Err(err) => {
                                        let msg = match err {
                                            crate::downloads::DownloadError::StorageLimitExceeded => {
                                                "Límite de almacenamiento alcanzado. Ajustá el límite o eliminá contenido.".to_owned()
                                            }
                                            other => format!("Error al iniciar descarga: {other}"),
                                        };
                                        download_manager.mark_job_failed(&job.id, &msg).await;
                                    }
                                }
                            } else {
                                download_manager
                                    .mark_job_failed(
                                        &job.id,
                                        "Sin fuentes disponibles para este capítulo. Hacé clic en el capítulo para elegir otra fuente o reintentar.",
                                    )
                                    .await;
                            }

                            crate::downloads::project_active_profile(ui_weak_inner.clone()).await;
                        }
                    }
                    return;
                }

                // Single movie or single episode download: always create a visible job immediately.
                let (ep_title, season_num, ep_num, video_id, dest_dir) = if is_series {
                    let s = current_season;
                    let e = current_ep_num;
                    let ep_name = current_ep_name;
                    let title_str = if !ep_name.is_empty() && ep_name != detail_title {
                        format!("{detail_title} - {ep_name} ({s}x{e})")
                    } else {
                        format!("{detail_title} ({s}x{e})")
                    };
                    let v_id = meta_id.as_ref().map(|m| format!("{m}:{s}:{e}"));
                    let dir = title_folder.join(format!("Temporada {:02}", s));
                    (title_str, Some(s), Some(e), v_id, dir)
                } else {
                    (detail_title.clone(), None, None, meta_id.clone(), title_folder)
                };

                crate::downloads::delete_stale_episode_jobs(
                    &download_manager,
                    &profile_id,
                    meta_id.as_deref(),
                    season_num,
                    ep_num,
                )
                .await;

                let sanitized_filename =
                    format!("{}.mp4", crate::downloads::sanitize_filename(&ep_title));
                let media_type = if is_series { "series" } else { "movie" };
                let placeholder = match download_manager
                    .create_placeholder_job(
                        &profile_id,
                        &ep_title,
                        &sanitized_filename,
                        &dest_dir,
                        crate::downloads::DownloadState::Resolving,
                        None,
                        crate::downloads::EnqueueOptions {
                            meta_id: meta_id.clone(),
                            meta_type: Some(media_type.into()),
                            series_name: Some(detail_title.clone()),
                            season: season_num,
                            episode: ep_num,
                            video_id: video_id.clone(),
                            poster_url: poster_url.clone(),
                            kind: Some("video".into()),
                            parent_job_id: None,
                            lang: None,
                        },
                    )
                    .await
                {
                    Ok(job) => {
                        crate::downloads::project_active_profile(ui_weak_inner.clone()).await;
                        job
                    }
                    Err(err) => {
                        let weak = ui_weak_inner.clone();
                        let msg = format!("No se pudo preparar la descarga: {err}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_downloads_error(msg.into());
                            }
                        });
                        return;
                    }
                };

                if let Some((url, final_stream, size_hint)) =
                    crate::downloads::resolve_stream_to_download_url(
                        selected.stream.clone(),
                        selected.stream_request.clone(),
                        selected.meta_request.clone(),
                        &streaming_server_url,
                        &settings,
                        &client,
                        false,
                    )
                    .await
                {
                    match download_manager
                        .activate_placeholder_job(
                            &placeholder,
                            crate::downloads::DownloadSource {
                                url,
                                headers: Vec::new(),
                                kind: crate::downloads::DownloadSourceKind::DirectHttp,
                            },
                            size_hint,
                        )
                        .await
                    {
                        Ok(()) => {
                            spawn_subtitles_for_job(
                                placeholder,
                                media_type,
                                video_id.unwrap_or_default(),
                                final_stream.subtitles,
                            );
                        }
                        Err(err) => {
                            let msg = match err {
                                crate::downloads::DownloadError::StorageLimitExceeded => {
                                    "Límite de almacenamiento de descargas alcanzado. Ajustá el límite o eliminá contenido.".to_string()
                                }
                                _ => format!("Error al iniciar descarga: {err}"),
                            };
                            download_manager.mark_job_failed(&placeholder.id, &msg).await;
                        }
                    }
                } else {
                    download_manager
                        .mark_job_failed(
                            &placeholder.id,
                            "No se pudo obtener un enlace descargable para esta fuente. Elegí otra fuente.",
                        )
                        .await;
                }
                crate::downloads::project_active_profile(ui_weak_inner).await;
            });
        }
    });

    // Compatibility callback
    ui.on_download_stream({
        let ui_weak = ui_weak.clone();
        move |selection_id| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_start_download(selection_id, "episode".into(), ui.get_detail_active_season());
            }
        }
    });

    ui.on_player_continue_as_torrent({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let server_readiness = server_readiness.clone();
        move || {
            let maybe_selected = PENDING_TORRENT_FALLBACK.lock().unwrap().take();
            if let Some(selected) = maybe_selected {
                let ui_weak = ui_weak.clone();
                let runtime = runtime.clone();
                let server_readiness = server_readiness.clone();
                tokio::spawn(async move {
                    if !server_readiness.is_ready() {
                        let _ = slint::invoke_from_event_loop({
                            let ui_weak = ui_weak.clone();
                            move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    ui.set_player_loading_stage_title("Iniciando motor de streaming…".into());
                                    ui.set_player_loading_stage_detail("Esperando conexión local de libtorrent…".into());
                                }
                            }
                        });
                        if !server_readiness.wait_ready(std::time::Duration::from_secs(6)).await {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    ui.set_player_loading_stage_title("Servidor de streaming no disponible".into());
                                    ui.set_player_loading_stage_detail(
                                        "El motor local tardó demasiado en iniciar. Reintenta la reproducción o revisa Ajustes.".into(),
                                    );
                                    ui.set_player_error(
                                        "No se pudo conectar con el motor de streaming local.".into(),
                                    );
                                }
                            });
                            return;
                        }
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_player_debrid_uncached(false);
                            ui.set_player_loading_stage_title("Conectando con enjambre…".into());
                            ui.set_player_loading_stage_detail("Buscando pares e intercambiando metadatos…".into());
                        }
                        runtime.dispatch(RuntimeAction {
                            field: None,
                            action: Action::Load(ActionLoad::Player(Box::new(selected))),
                        });
                    });
                });
            }
        }
    });

    let clipboard = Arc::new(Mutex::new(arboard::Clipboard::new().ok()));

    ui.on_play_url_magnet_from_clipboard({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let navigation = navigation.clone();
        let clipboard = clipboard.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let text = clipboard
                .lock()
                .ok()
                .and_then(|mut cb| cb.as_mut()?.get_text().ok())
                .unwrap_or_default();
            let trimmed = text.trim();
            if !trimmed.is_empty()
                && (trimmed.starts_with("magnet:")
                    || trimmed.starts_with("stremio:")
                    || trimmed.starts_with("http://")
                    || trimmed.starts_with("https://"))
            {
                crate::deep_link::handle(
                    crate::single_instance::AppCommand::Open(trimmed.to_owned()),
                    &ui,
                    &runtime,
                    &navigation,
                );
            } else {
                ui.set_error_message(
                    "Clipboard does not contain a valid URL or magnet link.".into(),
                );
            }
        }
    });

    ui.on_details_copy_stream_link({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let clipboard = clipboard.clone();
        move |selection_id| {
            let rt = runtime.clone();
            let playback_selections = playback_selections.clone();
            let clipboard = clipboard.clone();
            let selection_id = selection_id.to_string();
            tokio::spawn(async move {
                let model = rt.model().expect("model read failed");
                let settings = model.ctx.profile.settings.clone();
                let streaming_server_url = model.streaming_server.base_url.clone();
                drop(model);

                if let Some((selected, _)) = playback_selections.resolve(&selection_id) {
                    let sdl = stremio_core::deep_links::StreamDeepLinks::from((
                        &selected.stream,
                        streaming_server_url.as_ref(),
                        &settings,
                    ));
                    let link = sdl.external_player.streaming.clone().unwrap_or_else(|| {
                        match &selected.stream.source {
                            stremio_core::types::resource::StreamSource::Url { url } => {
                                url.to_string()
                            }
                            stremio_core::types::resource::StreamSource::YouTube { yt_id } => {
                                format!("https://youtube.com/watch?v={}", yt_id)
                            }
                            stremio_core::types::resource::StreamSource::Torrent {
                                info_hash,
                                ..
                            } => format!("magnet:?xt=urn:btih:{}", hex::encode(info_hash)),
                            _ => String::new(),
                        }
                    });
                    if !link.is_empty()
                        && let Ok(mut cb) = clipboard.lock()
                        && let Some(cb) = cb.as_mut()
                    {
                        let _ = cb.set_text(link);
                    }
                }
            });
        }
    });

    ui.on_details_copy_magnet_link({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let clipboard = clipboard.clone();
        let ui_weak = ui_weak.clone();
        move |selection_id| {
            let rt = runtime.clone();
            let playback_selections = playback_selections.clone();
            let clipboard = clipboard.clone();
            let ui_weak = ui_weak.clone();
            let selection_id = selection_id.to_string();
            tokio::spawn(async move {
                let model = rt.model().expect("model read failed");
                let settings = model.ctx.profile.settings.clone();
                let streaming_server_url = model.streaming_server.base_url.clone();
                drop(model);

                if let Some((selected, _)) = playback_selections.resolve(&selection_id) {
                    let sdl = stremio_core::deep_links::StreamDeepLinks::from((
                        &selected.stream,
                        streaming_server_url.as_ref(),
                        &settings,
                    ));
                    let link =
                        sdl.external_player.magnet.clone().unwrap_or_else(|| {
                            match &selected.stream.source {
                                stremio_core::types::resource::StreamSource::Torrent {
                                    info_hash,
                                    ..
                                } => format!("magnet:?xt=urn:btih:{}", hex::encode(info_hash)),
                                _ => String::new(),
                            }
                        });
                    if !link.is_empty() {
                        if let Ok(mut cb) = clipboard.lock()
                            && let Some(cb) = cb.as_mut()
                        {
                            let _ = cb.set_text(link);
                        }
                    } else {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_error_message(
                                    "No magnet link available for this stream.".into(),
                                );
                            }
                        });
                    }
                }
            });
        }
    });

    ui.on_details_copy_download_link({
        let runtime = runtime.clone();
        let playback_selections = playback_selections.clone();
        let ui_weak = ui_weak.clone();
        move |selection_id| {
            let rt = runtime.clone();
            let playback_selections = playback_selections.clone();
            let ui_weak = ui_weak.clone();
            let selection_id = selection_id.to_string();
            tokio::spawn(async move {
                let (settings, streaming_server_url) = {
                    let model = rt.model().expect("model read failed");
                    (
                        model.ctx.profile.settings.clone(),
                        model.streaming_server.base_url.clone(),
                    )
                };

                if let Some((selected, _stream_name)) = playback_selections.resolve(&selection_id) {
                    let sdl = stremio_core::deep_links::StreamDeepLinks::from((
                        &selected.stream,
                        streaming_server_url.as_ref(),
                        &settings,
                    ));
                    if let Some(link) = sdl.external_player.download {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                                let _ = clipboard.set_text(link.to_string());
                            }
                        });
                    } else {
                        show_download_error(
                            &ui_weak,
                            "No download link is available for this stream.".to_owned(),
                        );
                    }
                }
            });
        }
    });

    ui.on_open_external_url(|url| {
        let url = url.to_string();
        if let Err(error) = open::that(&url) {
            tracing::error!(%error, %url, "failed to open external url");
        }
    });

    ui.on_navigation_back({
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move || {
            crate::models::details::request_reset_projection_fingerprint();
            if let Some(ui) = ui_weak.upgrade() {
                navigation.dispatch_and_project(&ui, NavigationIntent::Back);
                ui.set_details_loading(false);
            }
        }
    });

    ui.on_navigation_forward({
        let ui_weak = ui_weak.clone();
        let navigation = navigation.clone();
        move || {
            crate::models::details::request_reset_projection_fingerprint();
            if let Some(ui) = ui_weak.upgrade() {
                navigation.dispatch_and_project(&ui, NavigationIntent::Forward);
                ui.set_details_loading(false);
            }
        }
    });

    ui.on_request_reload_current_view({
        let ui_weak = ui_weak.clone();
        let runtime = runtime.clone();
        let navigation = navigation.clone();
        move || {
            tracing::info!("user requested view refresh via F5");
            crate::models::details::request_reset_projection_fingerprint();
            crate::models::board::clear_sync_state();
            if let Some(ui) = ui_weak.upgrade() {
                let active_tab = navigation.active_tab_index();
                crate::sync_tab_from_model(
                    crate::navigation::Tab::try_from(active_tab).unwrap_or(crate::navigation::Tab::Board),
                    &runtime,
                    &ui,
                    &ui_weak,
                    &navigation,
                );
            }
            // Discharging an Unload on Board forces stremio-core to not discard the re-fetch
            runtime.dispatch(stremio_core::runtime::RuntimeAction {
                field: Some(crate::AppModelField::Board),
                action: stremio_core::runtime::msg::Action::Unload,
            });
            trigger_initial_load(&runtime);
            // Also directly poke the catalog visibility loader so visible rows load right away
            if let Some(loader) = crate::models::board::get_catalog_loader() {
                let _ = loader.try_send(0);
            }
        }
    });

    ui.on_toggle_fullscreen({
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let fs = !ui.window().is_fullscreen();
                ui.window().set_fullscreen(fs);
                ui.set_is_fullscreen(fs);
            }
        }
    });
}

fn project_current_streams(
    runtime: &Arc<Runtime<DesktopEnv, AppModel>>,
    playback_selections: &PlaybackSelections,
    ui_weak: &slint::Weak<MainWindow>,
) {
    let views = {
        let model = runtime.model().expect("model read failed");
        playback_selections.rebuild(&model.meta_details, &model.ctx.profile.addons)
    };
    if let Some(ui) = ui_weak.upgrade() {
        crate::event_loop::project_stream_selection_views(&ui, ui_weak, views);
    }
}

fn ranking_mode_name(mode: stream_ranking::RankingMode) -> &'static str {
    match mode {
        stream_ranking::RankingMode::Smart => "Smart",
        stream_ranking::RankingMode::Quality => "Quality",
        stream_ranking::RankingMode::Smallest => "Smallest",
        stream_ranking::RankingMode::Seeders => "Seeders",
        stream_ranking::RankingMode::Original => "Original",
    }
}

fn persist_profile_setting(key: &'static str, value: &'static str) {
    tokio::spawn(async move {
        match crate::profiles::active_profile_id().await {
            Ok(profile_id) => {
                if let Err(error) = crate::profiles::set_setting(&profile_id, key, value).await {
                    tracing::warn!(%error, setting = key, "failed to save profile setting");
                }
            }
            Err(error) => {
                tracing::warn!(%error, setting = key, "active profile is unavailable");
            }
        }
    });
}

fn show_download_error(ui_weak: &slint::Weak<MainWindow>, message: String) {
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_error_message(message.into());
        }
    });
}

#[allow(dead_code)]
fn download_filename(title: &str, link: &str) -> String {
    let extension = url::Url::parse(link)
        .ok()
        .and_then(|url| {
            Path::new(url.path())
                .extension()
                .and_then(|extension| extension.to_str())
                .map(ToOwned::to_owned)
        })
        .filter(|extension| extension.len() <= 8)
        .unwrap_or_else(|| "mp4".to_owned());
    format!("{title}.{extension}")
}

pub fn trigger_initial_load(runtime: &Arc<Runtime<DesktopEnv, AppModel>>) {
    runtime.dispatch(RuntimeAction {
        field: None,
        action: Action::Load(ActionLoad::CatalogWithFilters(None)),
    });
    runtime.dispatch(RuntimeAction {
        field: Some(AppModelField::Board),
        action: Action::Load(ActionLoad::CatalogsWithExtra(
            stremio_core::models::catalogs_with_extra::Selected {
                r#type: None,
                extra: vec![],
            },
        )),
    });
    // Immediately request the first rows so stremio-core does not wait for a scroll event
    runtime.dispatch(RuntimeAction {
        field: Some(AppModelField::Board),
        action: Action::CatalogsWithExtra(ActionCatalogsWithExtra::LoadRange(
            0..crate::models::CATALOG_PRELOAD_ROWS + 1,
        )),
    });
    if let Some(loader) = crate::models::board::get_catalog_loader() {
        let _ = loader.try_send(0);
    }
    runtime.dispatch(RuntimeAction {
        field: Some(AppModelField::LocalSearch),
        action: Action::Load(ActionLoad::LocalSearch),
    });
    runtime.dispatch(RuntimeAction {
        field: None,
        action: Action::Load(ActionLoad::LibraryWithFilters(
            stremio_core::models::library_with_filters::Selected {
                request: LibraryRequest {
                    r#type: None,
                    sort: Sort::LastWatched,
                    page: Default::default(),
                },
            },
        )),
    });
    runtime.dispatch(RuntimeAction {
        field: None,
        action: Action::Load(ActionLoad::InstalledAddonsWithFilters(
            stremio_core::models::installed_addons_with_filters::Selected {
                request: InstalledAddonsRequest { r#type: None },
            },
        )),
    });
}
