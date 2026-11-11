//! User-folder rules (spec §5.2g): store rules over `config.user_rules`.

use crate::category::store::scan_stores;
use crate::category::{Category, ScanCtx, ScanOutput};
use crate::fsutil::measure::InodeSet;

pub struct UserRules;

impl Category for UserRules {
    fn id(&self) -> &'static str {
        "user_rules"
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        Ok(scan_stores(self.id(), &ctx.config.user_rules, ctx, seen, &|n, days| {
            format!("{n} files older than {days} days")
        }, &|_| false))
    }
}
