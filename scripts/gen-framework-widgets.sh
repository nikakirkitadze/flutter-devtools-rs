#!/usr/bin/env bash
# Regenerates crates/fdt_analysis/src/framework_widgets.rs from a Flutter SDK checkout.
#
#   scripts/gen-framework-widgets.sh [flutter-sdk-dir]   (default: the `flutter` on PATH)
#
# Lists every class in packages/flutter/lib/src/{widgets,material,cupertino} that extends a
# Widget type, private classes included.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
sdk="${1:-$(cd "$(dirname "$(command -v flutter)")/.." && pwd)}"
lib="$sdk/packages/flutter/lib/src"
version="$(sed -n 's/.*"frameworkVersion": *"\([^"]*\)".*/\1/p' "$sdk/bin/cache/flutter.version.json" 2>/dev/null | head -1)"
version="${version:-unknown}"
out="$root/crates/fdt_analysis/src/framework_widgets.rs"

bases='Widget|StatelessWidget|StatefulWidget|InheritedWidget|ProxyWidget|ParentDataWidget|RenderObjectWidget|SingleChildRenderObjectWidget|MultiChildRenderObjectWidget|LeafRenderObjectWidget|InheritedTheme|InheritedNotifier|InheritedModel'
# Every `class A extends B` pair, then the closure over classes that reach a Widget base
# (Column extends Flex extends MultiChildRenderObjectWidget).
names="$(cat "$lib"/widgets/*.dart "$lib"/material/*.dart "$lib"/cupertino/*.dart \
  | perl -0777 -ne '
      my %base = map { $_ => 1 } split /\|/, "'"$bases"'";
      my %ext;
      while (/\b(?:abstract\s+|base\s+|final\s+|sealed\s+)*class\s+(\w+)\b(?:<[^{;]*?>)?\s+extends\s+(\w+)/g) { $ext{$1} = $2 }
      my $changed = 1;
      my %w;
      while ($changed) {
        $changed = 0;
        for my $c (keys %ext) {
          next if $w{$c};
          if ($base{$ext{$c}} || $w{$ext{$c}}) { $w{$c} = 1; $changed = 1 }
        }
      }
      print "$_\n" for keys %w;' \
  | LC_ALL=C sort -u)"

{
  echo "//! Widget classes of the Flutter framework (widgets, material, cupertino), sorted."
  echo "//!"
  echo "//! Generated from Flutter $version. Regenerate with:"
  echo "//!"
  echo '//! `scripts/gen-framework-widgets.sh [flutter-sdk-dir]`'
  echo
  echo "/// Sorted bytewise, for a binary search."
  echo "pub static FRAMEWORK_WIDGETS: &[&str] = &["
  while read -r n; do echo "    \"$n\","; done <<<"$names"
  echo "];"
  cat <<'RS'

/// Whether `name` (a widget `runtimeType`, generics allowed) is a framework widget.
pub fn is_framework_widget(name: &str) -> bool {
    let base = name.split('<').next().unwrap_or(name);
    FRAMEWORK_WIDGETS.binary_search(&base).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_is_sorted_and_has_the_common_widgets() {
        assert!(FRAMEWORK_WIDGETS.windows(2).all(|w| w[0] < w[1]));
        for n in [
            "Scaffold",
            "Column",
            "Wrap",
            "Expanded",
            "SingleChildScrollView",
            "SizedBox",
            "AppBar",
            "_ScaffoldScope",
        ] {
            assert!(is_framework_widget(n), "{n}");
        }
        assert!(!is_framework_widget("ProductGrid"));
        assert!(!is_framework_widget("CartScreen"));
    }

    #[test]
    fn generic_names_match_on_the_part_before_the_bracket() {
        assert!(is_framework_widget("NotificationListener<LayoutChangedNotification>"));
        assert!(!is_framework_widget("MyBox<int>"));
    }
}
RS
} >"$out"
rustfmt --edition 2024 "$out"
echo "wrote $(grep -c '^    "' "$out") widgets to $out (Flutter $version)"
