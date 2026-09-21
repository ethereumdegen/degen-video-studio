//! `dvs export <format> <out>`: hand the edit to another tool.
//!
//! One op per format rather than one op with a format switch, because the writers share
//! nothing but the output path — `.kdenlive` carries `kdenlive:*` document keys that
//! plain MLT must not have, and an EDL cannot express what FCPXML can. The exporters are
//! query ops: they write a file and journal nothing, since exporting does not change the
//! edit.

use crate::cli::ExportArgs;
use crate::commands::Ctx;
use dvs_core::error::Result;
use serde_json::json;

pub fn run(ctx: &Ctx, args: ExportArgs) -> Result<()> {
    ctx.apply(
        args.format.op_id(),
        json!({ "out": args.out.display().to_string() }),
    )
    .map(|_| ())
}
