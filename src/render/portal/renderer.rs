use super::*;

pub(super) type RenderClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Production renderer for every page behind the frozen [`PageRenderer`] port.
#[derive(Clone)]
pub struct AskamaPageRenderer {
    pub(super) app_name: String,
    pub(super) app_brand: String,
    pub(super) site_host: String,
    pub(super) clock: RenderClock,
    pub(super) audit_key: Option<[u8; 32]>,
}

impl AskamaPageRenderer {
    #[must_use]
    pub fn from_config(config: &AppConfig) -> Self {
        let mut renderer = Self::new(
            config.app_name.clone(),
            config.app_brand.clone(),
            &config.public_base_url,
        );
        renderer.audit_key = config
            .audit_ledger_hmac_key
            .as_ref()
            .and_then(|v| parse_hmac_key(v.expose()).ok());
        renderer
    }

    #[must_use]
    pub fn new(app_name: String, app_brand: String, public_base_url: &str) -> Self {
        Self {
            app_name,
            app_brand,
            site_host: site_host(public_base_url),
            clock: Arc::new(system_now_unix_seconds),
            audit_key: None,
        }
    }

    /// Deterministic rendering seam for fixed-clock snapshots.
    #[must_use]
    pub fn with_fixed_clock(config: &AppConfig, unix_seconds: i64) -> Self {
        let mut renderer = Self::from_config(config);
        renderer.clock = Arc::new(move || unix_seconds);
        renderer
    }
}

impl Default for AskamaPageRenderer {
    fn default() -> Self {
        Self::from_config(&AppConfig::default())
    }
}

pub(super) fn system_now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

pub(super) fn site_host(public_base_url: &str) -> String {
    Url::parse(public_base_url)
        .ok()
        .map(|url| url[Position::BeforeHost..Position::AfterPort].to_owned())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "localhost:3480".to_owned())
}
pub(super) fn render<T: Template>(template: &T) -> Result<String, AppError> {
    template.render().map_err(|_| AppError::Internal)
}

impl PageRenderer for AskamaPageRenderer {
    fn gallery(&self, view: &GalleryView) -> Result<String, AppError> {
        render(&gallery_template(self, view))
    }

    fn shell(&self, view: &ShellView) -> Result<String, AppError> {
        render(&shell_template(self, view)?)
    }

    fn settings(&self, view: &SettingsView) -> Result<String, AppError> {
        crate::render::settings::render_settings(
            &self.site_host,
            &self.app_name,
            &self.app_brand,
            view,
        )
    }

    fn not_found(&self, message: Option<&str>) -> Result<String, AppError> {
        render(&NotFoundTemplate {
            favicon: FAVICON,
            theme_boot: THEME_BOOT,
            app_name: &self.app_name,
            app_brand: &self.app_brand,
            site_host: &self.site_host,
            message: message
                .filter(|message| !message.is_empty())
                .unwrap_or("It may have been deleted, or the link is no longer valid."),
            css: NOT_FOUND_CSS,
        })
    }

    fn not_signed_in(&self) -> Result<String, AppError> {
        render(&NotSignedInTemplate {
            favicon: FAVICON,
            theme_boot: THEME_BOOT,
            app_name: &self.app_name,
            app_brand: &self.app_brand,
            site_host: &self.site_host,
            css: NOT_SIGNED_IN_CSS,
        })
    }

    fn access_retry(&self, target: &str) -> Result<String, AppError> {
        render(&AccessRetryTemplate {
            favicon: FAVICON,
            theme_boot: THEME_BOOT,
            app_name: &self.app_name,
            app_brand: &self.app_brand,
            site_host: &self.site_host,
            target: if target.is_empty() {
                "/?cf_access_retry=1"
            } else {
                target
            },
            css: ACCESS_RETRY_CSS,
        })
    }
}
