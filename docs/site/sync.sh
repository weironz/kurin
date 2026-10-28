#!/bin/sh
# 把 ../../（docs/）下的 Markdown 拷进 mdBook 的 src/（排除工作台账）。
# src/SUMMARY.md 是手写的目录，不会被覆盖；新增文档时记得同步加一行。
set -eu
cd "$(dirname "$0")"

# 清掉上一次的拷贝（保留手写的 src/SUMMARY.md）
mkdir -p src
find src -mindepth 1 -maxdepth 1 ! -name SUMMARY.md -exec rm -rf {} +
mkdir -p src/architecture/spikes src/ui

# docs 根目录（progress.md 是工作台账，不进站点）
for f in ../*.md; do
  case "$f" in
    ../progress.md) ;;
    *) cp "$f" src/ ;;
  esac
done

# 分类目录
for d in getting-started guides reference operations docker development project; do
  mkdir -p "src/$d"
  for f in ../"$d"/*.md; do
    case "$f" in
      ../project/progress.md) continue ;;
    esac
    cp "$f" "src/$d/"
  done
done

# 架构文档（PROGRESS.md / HANDOFF.md 是台账，不进站点）
for f in ../architecture/*.md; do
  case "$f" in
    ../architecture/PROGRESS.md | ../architecture/HANDOFF.md) ;;
    *) cp "$f" src/architecture/ ;;
  esac
done

cp ../architecture/spikes/30-oci.md src/architecture/spikes/
cp ../ui/*.md src/ui/

# JSON Schema（文档里的相对链接）
mkdir -p src/schema
cp ../schema/*.json src/schema/

echo "synced $(find src -name '*.md' ! -name SUMMARY.md | wc -l) pages into docs/site/src/"