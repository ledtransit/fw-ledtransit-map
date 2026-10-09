// The setup portal's files, built by the prov_server tool into assets/prov_public

pub struct StaticFile {
    pub path: &'static str,
    pub content: &'static [u8],
    pub mime_type: &'static str,
    pub locale: &'static str,
    /// Stored gzip compressed, served as is with Content-Encoding: gzip
    pub gzip: bool,
}

macro_rules! asset_path {
    ($file:expr) => {
        concat!(env!("CARGO_MANIFEST_DIR"), "/assets/prov_public/", $file)
    };
}

const DEFAULT_LOCALE: &str = "en";

const STATIC_FILES: &[StaticFile] = &[
    StaticFile {
        path: "/setup-wifi",
        content: include_bytes!(asset_path!("setup-wifi+en.html.gz")),
        mime_type: "text/html",
        locale: "en",
        gzip: true,
    },
    StaticFile {
        path: "/setup-wifi",
        content: include_bytes!(asset_path!("setup-wifi+de.html.gz")),
        mime_type: "text/html",
        locale: "de",
        gzip: true,
    },
    StaticFile {
        path: "/styles.css",
        content: include_bytes!(asset_path!("styles.css.gz")),
        mime_type: "text/css",
        locale: DEFAULT_LOCALE,
        gzip: true,
    },
    StaticFile {
        path: "/favicon.ico",
        content: include_bytes!(asset_path!("favicon.ico")),
        mime_type: "image/x-icon",
        locale: DEFAULT_LOCALE,
        gzip: false,
    },
    StaticFile {
        path: "/favicon.svg",
        content: include_bytes!(asset_path!("favicon.svg.gz")),
        mime_type: "image/svg+xml",
        locale: DEFAULT_LOCALE,
        gzip: true,
    },
    StaticFile {
        path: "/background.webp",
        content: include_bytes!(asset_path!("background.webp")),
        mime_type: "image/webp",
        locale: DEFAULT_LOCALE,
        gzip: false,
    },
];

/// The file at the path in the locale, else in the default locale.
pub fn find(path: &str, locale: Option<&str>) -> Option<&'static StaticFile> {
    let find_in = |locale: &str| {
        STATIC_FILES
            .iter()
            .find(|file| file.path == path && file.locale == locale)
    };
    find_in(locale.unwrap_or(DEFAULT_LOCALE)).or_else(|| find_in(DEFAULT_LOCALE))
}
