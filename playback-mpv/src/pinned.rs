// Minimum client API the bindings require. The pinned Windows runtime in
// `mpv.lock.json` is newer (2.6 for mpv 0.42), but mpv 0.41 (2.5) remains
// supported because no 2.6-only entry point is used.
pub const HEADER_CLIENT_API_VERSION: ApiVersion = ApiVersion::new(2, 5);
