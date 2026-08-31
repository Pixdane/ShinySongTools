# resources-route: 离线解析下载资源(歌词/文本)

目的:绕过 in-process hook,直接离线解析下载存储中的资源文件。

## 存储形态(2026-08-30 实测)

`Documents/D/<2ch>/<hash>` 内容寻址存储,185,655 文件 / 22GB:

| 形态 | 文件数 | 体积 | 说明 |
|---|---|---|---|
| UnityFS(前缀 4 字节常量 0x1BA) | 136,588 | 12.2GB | Unity 6000.1.16f 资产包,**明文** |
| LZ4 frame(魔数 0x184D2204) | 32,322 | 112MB | MV 演出 JSON(站位/镜头/口型),纯 Python 可解,**无歌词** |
| @UFF / CRID / AFS2 | ~10.5k | 10.5GB | CRIWARE 音视频 |

## 结论(2026-08-31 实验完成)

**Unity 6000.1.16f1 archive version-8 头格式已破解并全量验证,歌词离线
全量提取路线打通。**

### v8 头布局(真机样本逐字节验证;与 v6/v7 完全不同,无 typetree)

全部 BE,以 2907 字节样本 `D/3K/3KB45VRU…` 为准:

```text
  0: u32 LE wrapper 前缀(语义未定,忽略)
  4: "UnityFS\0"
 12: u32 version = 8
 16: cstring unity("5.x.x\0") + cstring engine("6000.1.16f1\0")
 34: u32 target platform = 0
 38: u32 header_size(从 BUNDLE 起点数,== 文件长度 - 4)
 42: u32 compressed blocksinfo size(65 / 89 / 88)
 46: u32 uncompressed blocksinfo size(91 / 121 / 153)
 50: u32 flags = 0x243(压缩 = LZ4;bit6 = 1)
 68: LZ4 raw-block 压缩的 blocksinfo(固定字段结束 54 之后 + 14 字节 pad)
```

- blocksinfo 明文:16 字节 hash 全零,`u32 block_count @16`,块表
  `{u32 uncomp; u32 comp; u16 flags}` BE stride 10(样本全部 131072 块)。
- 数据块紧随压缩 blocksinfo 之后:comp0@`info_end + pad`(样本 1 实测
  pad=7,另一小样本 pad=15)。**pad 并不固定**,扫描器对每个文件用
  "首块 LZ4 解压成功 + 总长不越界"校验自动选 pad(0..=24 候选);
  info pad 同理带回退(默认 14)。
- `header_size`、`flags@50` 等与 UnityPy 的 v7 解析对不上,不要参考。

### 全量扫描结果

- `STATS=1`:185,655 文件中 136,588 个 UnityFS,
  **header-parsed = fully-decompressed = 136,588(100%)**。
- 歌词特征扫描:**118 个 bundle** 命中(全部为 `lyric` 对象名 + 附近
  UTF-8 日文歌词行)。与 176 首 MV 数量级吻合(偏少:部分歌共用
  bundle、部分歌词文本不在 lyric 名 8KB 窗口内、部分 MV 未下载)。
- 《Spread the Wings》验证:`D/BR/BRAYTVRND7X2JYEOVFUIWALXLA`
  命中,解压数据同时含 `誰も見たことない翼` 与真实歌词行
  (`Spread the Wings　行こう　今、羽ばたく時間` 等)。
- 字体图集(假名/汉字码表 UTF-16 段)是大假阳性源;已用"歌词行形态"
  过滤(短 + 假名/汉字混用)排除。

### 后续方向

- 对 118 个命中 bundle 做完整对象级解析(blocksinfo 之后还有 node 表,
  明文里 `p+10*i` 起为节点信息),把每首歌的歌词按行序提取成 JSON。
- `lyric` 对象名命中点的文本窗口提取只是定位手段;精确提取需解析
  序列化对象(m_Text 字段),或直接对窗口做更完整的 UTF-8 字符串扫描。

## 用法

```sh
cd experiments/resources-route
cargo build --release
../../build/target/release/resources-route <D目录或单个bundle> [文本过滤]
# STATS=1 输出解析/解压统计;无参数打印 usage
```

单文件模式可带子串过滤(在解压数据中搜索并打印上下文);全量模式输出
命中 bundle 路径 + 提取的歌词行(样本输出见
`build/experiments/resources-route/lyric_hits.txt`)。
