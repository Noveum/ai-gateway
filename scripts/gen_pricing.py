#!/usr/bin/env python3
"""Generate every pricing artifact from the one versioned catalog.

`pricing/catalog.json` is the single source of truth for LLM rates. This script
renders it into the two consumers:

  * `src/policy/pricing_catalog.rs`  - the gateway's Rust rate rows
  * `packages/telemetry/src/pricing/generated/index.ts` (in the platform repo)
    - the platform's TypeScript rate rows, plus a byte-identical copy of the
      catalog next to them so the platform can verify itself with no access to
      this repo.

Neither artifact is hand-editable; both carry a generated banner and the
catalog's version and SHA-256, so any cost can be traced back to the exact rate
card that produced it.

Usage:
    scripts/gen_pricing.py                       # write the Rust artifact
    scripts/gen_pricing.py --platform-repo PATH  # ...and the TypeScript one
    scripts/gen_pricing.py --check               # fail if anything has drifted

`--check` is what CI runs: it re-renders every artifact in memory and diffs it
against what is committed, so an edit to a generated file (or to the catalog
without regenerating) fails the build instead of silently diverging.
"""

from __future__ import annotations

import argparse
import difflib
import hashlib
import json
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CATALOG = REPO / "pricing" / "catalog.json"
RUST_OUT = REPO / "src" / "policy" / "pricing_catalog.rs"
TS_REL = Path("packages/telemetry/src/pricing/generated/index.ts")
TS_CATALOG_REL = Path("packages/telemetry/src/pricing/generated/catalog.json")
# Must track `edition` in Cargo.toml so the generated file formats identically
# to what `cargo fmt --all -- --check` expects.
RUST_EDITION = "2021"

BANNER = (
    "GENERATED FILE - DO NOT EDIT.\n"
    "Source of truth: pricing/catalog.json in the noveum/ai-gateway repo.\n"
    "Regenerate with: scripts/gen_pricing.py\n"
    "CI fails on drift (scripts/gen_pricing.py --check)."
)


def load_catalog() -> tuple[dict, str, str]:
    raw = CATALOG.read_bytes()
    return json.loads(raw), hashlib.sha256(raw).hexdigest(), raw.decode()


def plain(value: float) -> str:
    """Render a rate in plain decimal, never in exponent notation.

    Rates get small once converted to per-1K units (a cached GPT-5 nano token is
    5e-06), and `repr` would emit exponent form for exactly those rows. Plain
    decimal keeps the two artifacts readable and diffable by eye, which matters
    for a file whose only job is to be audited.
    """
    text = f"{float(value):.12f}".rstrip("0").rstrip(".")
    return text or "0"


def num(value: float) -> str:
    """A Rust `f64` literal: plain decimal, always carrying a decimal point."""
    text = plain(value)
    return text if "." in text else f"{text}.0"


def rust_opt(value) -> str:
    return "None" if value is None else f"Some({num(value)})"


def render_rust(cat: dict, sha: str) -> str:
    rows = [m for m in cat["models"] if "rust" in m["targets"]]
    out: list[str] = []
    w = out.append

    for line in BANNER.splitlines():
        w(f"// {line}")
    w("")
    w("//! Published LLM rates, generated from the versioned pricing catalog.")
    w("//!")
    w("//! Every rate is USD per 1,000,000 tokens. `cached_input_per_1m` prices a")
    w("//! prompt-cache HIT and `cache_write_per_1m` prices tokens written INTO the")
    w("//! cache; `None` means the provider publishes no rate for that dimension, so")
    w("//! [`crate::policy::pricing`] reports it as a missing dimension instead of")
    w("//! charging zero for it. `Some(0.0)` is different: it means the provider")
    w("//! documents that the dimension is genuinely free.")
    w("//!")
    w("//! Rows are limited to models the gateway can actually route to. The catalog")
    w("//! also carries the platform's historical rows; those render only into the")
    w("//! TypeScript artifact (see `targets` in the catalog).")
    w("")
    w("/// The rate card these rows came from. Recorded on every usage and")
    w("/// reservation record so a billed amount can be traced back to it.")
    w(f'pub const CATALOG_VERSION: &str = "{cat["version"]}";')
    w("")
    w("/// SHA-256 of `pricing/catalog.json`. Pins this artifact to the exact bytes")
    w("/// it was rendered from; a test re-hashes the catalog and fails on a mismatch.")
    w(f'pub const CATALOG_SHA256: &str = "{sha}";')
    w("")
    w("/// One model's published rates.")
    w("#[derive(Debug, Clone, Copy, PartialEq)]")
    w("pub struct CatalogRow {")
    w("    pub id: &'static str,")
    w("    pub provider: &'static str,")
    w("    pub input_per_1m: f64,")
    w("    pub output_per_1m: f64,")
    w("    /// Prompt-cache HIT rate. `None` when the provider publishes none.")
    w("    pub cached_input_per_1m: Option<f64>,")
    w("    /// Cache-write rate at the provider's default TTL. `Some(0.0)` means")
    w("    /// documented-free; `None` means unpublished.")
    w("    pub cache_write_per_1m: Option<f64>,")
    w("    /// Anthropic's 1-hour-TTL cache write (2x base). `None` when the")
    w("    /// provider has a single write rate.")
    w("    pub cache_write_1h_per_1m: Option<f64>,")
    w("}")
    w("")
    w(f"/// {len(rows)} models the gateway prices from the catalog.")
    w("pub const MODEL_ROWS: &[CatalogRow] = &[")
    for m in rows:
        w("    CatalogRow {")
        w(f'        id: "{m["id"]}",')
        w(f'        provider: "{m["provider"]}",')
        w(f"        input_per_1m: {num(m['inputPer1m'])},")
        w(f"        output_per_1m: {num(m['outputPer1m'])},")
        w(f"        cached_input_per_1m: {rust_opt(m['cachedInputPer1m'])},")
        w(f"        cache_write_per_1m: {rust_opt(m['cacheWritePer1m'])},")
        w(f"        cache_write_1h_per_1m: {rust_opt(m['cacheWrite1hPer1m'])},")
        w("    },")
    w("];")
    w("")
    w("/// Bare provider aliases resolved server-side, `(alias, concrete_id)`.")
    w("pub const MODEL_ALIASES: &[(&str, &str)] = &[")
    for a in cat["aliases"]:
        w(f'    ("{a["alias"]}", "{a["target"]}"),')
    w("];")
    w("")
    w("/// A documented long-context tier: above `threshold_input_tokens` the WHOLE")
    w("/// request bills at these rates instead of the model's base row.")
    w("#[derive(Debug, Clone, Copy, PartialEq)]")
    w("pub struct LongContextRow {")
    w("    pub id: &'static str,")
    w("    pub threshold_input_tokens: u32,")
    w("    pub input_per_1m: f64,")
    w("    pub output_per_1m: f64,")
    w("    pub cached_input_per_1m: Option<f64>,")
    w("    pub cache_write_per_1m: Option<f64>,")
    w("}")
    w("")
    w("pub const LONG_CONTEXT_ROWS: &[LongContextRow] = &[")
    for r in cat["longContext"]:
        w("    LongContextRow {")
        w(f'        id: "{r["id"]}",')
        w(f"        threshold_input_tokens: {r['thresholdInputTokens']},")
        w(f"        input_per_1m: {num(r['inputPer1m'])},")
        w(f"        output_per_1m: {num(r['outputPer1m'])},")
        w(f"        cached_input_per_1m: {rust_opt(r['cachedInputPer1m'])},")
        w(f"        cache_write_per_1m: {rust_opt(r['cacheWritePer1m'])},")
        w("    },")
    w("];")
    w("")
    w("/// An announced, already-published future price change. The latest entry")
    w("/// whose `effective_from_unix_secs` has passed replaces the base row.")
    w("#[derive(Debug, Clone, Copy, PartialEq)]")
    w("pub struct ScheduledRow {")
    w("    pub id: &'static str,")
    w("    pub effective_from_unix_secs: i64,")
    w("    pub input_per_1m: f64,")
    w("    pub output_per_1m: f64,")
    w("}")
    w("")
    w("pub const SCHEDULED_ROWS: &[ScheduledRow] = &[")
    for r in cat["scheduled"]:
        w("    ScheduledRow {")
        w(f'        id: "{r["id"]}",')
        w(f"        effective_from_unix_secs: {r['effectiveFromUnixSecs']},")
        w(f"        input_per_1m: {num(r['inputPer1m'])},")
        w(f"        output_per_1m: {num(r['outputPer1m'])},")
        w("    },")
    w("];")
    w("")
    w("/// Per-request tool and search fees, `(tool_id, usd_per_1000_calls)`. Tool")
    w("/// ids are `provider:tool` or `provider:model:tool`.")
    w("pub const TOOL_FEES_USD_PER_1K_CALLS: &[(&str, f64)] = &[")
    for t in cat["toolFees"]:
        w(f'    ("{t["id"]}", {num(t["usdPer1kCalls"])}),')
    w("];")
    w("")
    w("/// Providers whose response body carries a BILLED total in USD that may be")
    w("/// trusted in place of the gateway's own arithmetic. Empty by design: no")
    w("/// provider in this catalog returns one today (Perplexity explicitly")
    w("/// disclaims the figure it does return). Adding an id here is what switches")
    w("/// a provider onto the authoritative-total path.")
    w("pub const AUTHORITATIVE_COST_PROVIDERS: &[&str] = &[")
    for p in cat["authoritativeCostProviders"]:
        w(f'    "{p}",')
    w("];")
    w("")
    return "\n".join(out)


def ts_key(key: str) -> str:
    """Quote a record key the way Biome formats it."""
    bare = key and (key[0].isalpha() or key[0] == "_")
    if bare and all(c.isalnum() or c == "_" for c in key):
        return key
    return f'"{key}"'


def per_1k(value: float) -> str:
    """Convert a per-1M rate to the per-1K units the platform table uses."""
    return plain(round(float(value) / 1000.0, 12))


def render_ts(cat: dict, sha: str) -> str:
    rows = [m for m in cat["models"] if "typescript" in m["targets"]]
    out: list[str] = []
    w = out.append

    # A `/*!` banner survives the repo's comment stripper, and this whole
    # directory is additionally excluded from it by the `/generated/` rule.
    w("/*!")
    for line in BANNER.splitlines():
        w(f" * {line}")
    w(" *")
    w(" * Rates are USD per 1,000 tokens, matching the units this package has")
    w(" * always used. The catalog stores per 1,000,000; the generator divides.")
    w(" */")
    w("")
    w("export interface GeneratedModelRates {")
    w("\tinput: number;")
    w("\toutput: number;")
    w("\taverage: number;")
    w("\tcachedInput?: number;")
    w("\tcacheWrite?: number;")
    w("\tcacheWrite1h?: number;")
    w("}")
    w("")
    w(f'export const PRICING_CATALOG_VERSION = "{cat["version"]}";')
    w("")
    w(f'export const PRICING_CATALOG_SHA256 =\n\t"{sha}";')
    w("")
    w("export const GENERATED_MODEL_PRICING: Record<string, GeneratedModelRates> = {")
    for m in rows:
        w(f"\t{ts_key(m['platformKey'])}: {{")
        w(f"\t\tinput: {per_1k(m['inputPer1m'])},")
        w(f"\t\toutput: {per_1k(m['outputPer1m'])},")
        w(f"\t\taverage: {per_1k(m['averagePer1m'])},")
        if m["cachedInputPer1m"] is not None:
            w(f"\t\tcachedInput: {per_1k(m['cachedInputPer1m'])},")
        if m["cacheWritePer1m"] is not None:
            w(f"\t\tcacheWrite: {per_1k(m['cacheWritePer1m'])},")
        if m["cacheWrite1hPer1m"] is not None:
            w(f"\t\tcacheWrite1h: {per_1k(m['cacheWrite1hPer1m'])},")
        w("\t},")
    w("};")
    w("")
    w("export const GENERATED_TOOL_FEES_USD_PER_1K_CALLS: Record<string, number> = {")
    for t in cat["toolFees"]:
        w(f"\t{ts_key(t['id'])}: {plain(t['usdPer1kCalls'])},")
    w("};")
    w("")
    return "\n".join(out)


def rustfmt(source: str) -> str:
    """Run the rendered Rust through rustfmt.

    Without this the artifact and `cargo fmt --all -- --check` disagree the
    moment a table has exactly one row (rustfmt collapses a single-element slice
    onto one line), which would make the drift check and the format check
    impossible to satisfy at the same time. Formatting here means the committed
    file is rustfmt-stable by construction.
    """
    try:
        done = subprocess.run(
            ["rustfmt", "--edition", RUST_EDITION, "--emit", "stdout", "--quiet"],
            input=source,
            capture_output=True,
            text=True,
            check=True,
        )
    except FileNotFoundError:
        print(
            "rustfmt not found; it is required so the generated artifact matches "
            "`cargo fmt --all -- --check`.",
            file=sys.stderr,
        )
        raise SystemExit(2) from None
    except subprocess.CalledProcessError as exc:
        print(f"rustfmt rejected the generated source:\n{exc.stderr}", file=sys.stderr)
        raise SystemExit(2) from None
    return done.stdout


def artifacts(platform_repo: Path | None) -> list[tuple[Path, str]]:
    cat, sha, raw = load_catalog()
    built: list[tuple[Path, str]] = [(RUST_OUT, rustfmt(render_rust(cat, sha)))]
    if platform_repo is not None:
        built.append((platform_repo / TS_REL, render_ts(cat, sha)))
        built.append((platform_repo / TS_CATALOG_REL, raw))
    return built


def check(built: list[tuple[Path, str]]) -> int:
    failures = 0
    for path, want in built:
        have = path.read_text() if path.exists() else ""
        if have == want:
            print(f"ok    {path}")
            continue
        failures += 1
        print(f"DRIFT {path}", file=sys.stderr)
        diff = difflib.unified_diff(
            have.splitlines(keepends=True),
            want.splitlines(keepends=True),
            fromfile=f"{path} (committed)",
            tofile=f"{path} (regenerated from pricing/catalog.json)",
            n=2,
        )
        sys.stderr.writelines(list(diff)[:80])
    if failures:
        print(
            f"\n{failures} generated pricing artifact(s) do not match "
            f"pricing/catalog.json.\nRun scripts/gen_pricing.py to regenerate, "
            f"then commit the result.",
            file=sys.stderr,
        )
    return 1 if failures else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--platform-repo", type=Path, default=None,
                    help="path to a noveum-app-nextjs checkout")
    ap.add_argument("--check", action="store_true",
                    help="verify committed artifacts instead of writing them")
    args = ap.parse_args()

    built = artifacts(args.platform_repo)
    if args.check:
        return check(built)
    for path, text in built:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
