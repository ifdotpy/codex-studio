//! Replace the group lookup index with a covering index without rebuilding facts.

/// Group lookup and write identity prefix, followed by the remaining read columns.
pub(crate) const INDEX: &str = "CREATE INDEX IF NOT EXISTS analytics_cost_groups_cover
    ON analytics_cost_groups(root,model,account,input,cached,cache_write,output,uncached,cache_write_1h,count);";
