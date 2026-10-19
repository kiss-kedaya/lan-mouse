#!/bin/sh
# Regenerate po/lan-mouse.pot from the sources listed in po/POTFILES.in.
set -eu
cd "$(dirname "$0")/.."

rust_sources=$(grep -v '\.ui$' po/POTFILES.in)
ui_sources=$(grep '\.ui$' po/POTFILES.in || true)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# messages from the Rust sources (gettext!("...") and .gettext("..."))
xgettext --no-wrap --add-comments --sort-output \
  --from-code=UTF-8 --keyword=gettext --keyword=gettext_noop \
  -o "$tmp/rust.po" $rust_sources

# messages from the GTK templates (translatable="yes")
if [ -n "$ui_sources" ]; then
  xgettext --no-wrap --add-comments --sort-output --language=Glade \
    --from-code=UTF-8 -o "$tmp/ui.po" $ui_sources
else
  : > "$tmp/ui.po"
fi

if [ -f po/zh_CN.po ]; then
  msgmerge --quiet --update po/zh_CN.po po/lan-mouse.pot
else
  msginit --no-translator --locale=zh_CN --input=po/lan-mouse.pot --output=po/zh_CN.po >/dev/null
  sed -i '' -e 's/^"Content-Type: .*/"Content-Type: text\/plain; charset=UTF-8\\n"/' po/zh_CN.po
fi
