//! User-folder rules (spec §5.2g): store rules over `config.user_rules`.

use crate::category::store::scan_stores;
use crate::category::{Category, ScanCtx, ScanOutput};
use crate::fmt::plural;
use crate::fsutil::measure::InodeSet;

pub struct UserRules;

impl Category for UserRules {
    fn id(&self) -> &'static str {
        "user_rules"
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        Ok(scan_stores(
            self.id(),
            &ctx.config.user_rules,
            ctx,
            seen,
            &|n, days| format!("{} older than {}", plural(n, "file"), plural(days, "day")),
            &|_| false,
        ))
    }

    fn explain(&self, path: &std::path::Path, ctx: &ScanCtx) -> Option<crate::category::Why> {
        crate::category::store::explain_stores(&ctx.config.user_rules, path, ctx)
    }
}
