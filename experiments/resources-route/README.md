# resources-route: 离线解析下载资源(歌词/文本)

目的:绕过 in-process hook,直接离线解析下载存储中的资源文件。

## 存储形态(2026-08-30 实测)

`Documents/D/<2ch>/<hash>` 内容寻址存储,184,915 文件 / 22GB:

| 形态 | 文件数 | 体积 | 说明 |
|---|---|---|---|
| UnityFS(前缀 4 字节常量 0x1BA) | 135,894 | 12.2GB | Unity 6000.1.16f 资产包,**明文** |
| LZ4 frame(魔数 0x184D2204) | 32,322 | 112MB | MV 演出 JSON(站位/镜头/口型),纯 Python 可解,**无歌词** |
| @UFF / CRID / AFS2 | ~10.5k | 10.5GB | CRIWARE 音视频 |

## 结论与卡点

- LZ4 数据文件全部解出:只有 MV 演出数据,无歌词/剧情。
- UnityFS 是 **archive version 8**(Unity 6 新头格式),按 UnityPy 的
  version-7 布局解析字段对不上(文件头前的 4 字节常量 `0x1BA` 语义未知,
  疑似 wrapper)。扫描器(`src/main.rs`,rayon 并行 + lz4_flex + lzma-rs)
  框架已就绪,需要 Unity 6 archive version-8 的精确头布局(参考 UnityPy
  master 对 Unity 6000 的支持)才能解出块数据。
- 歌词不在 DataFile 已捕获集合中;`TimelineController.SetLyric` 在 iOS
  零直接调用者。歌词的实际读取路径仍未知。

## 用法

```sh
cd experiments/resources-route
cargo build --release
./../../build/target/release/resources-route <D目录> [sid过滤]
```
