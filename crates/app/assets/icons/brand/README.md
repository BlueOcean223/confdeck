# 品牌图标

这些 SVG 复制自 [yetone/magpie](https://github.com/yetone/magpie) 的
`internal/gui/assets/icons/`，该仓库以 MIT 协议发布（Copyright (c) 2026 yetone）。
图标风格与 LobeHub Icons 一致，原始出处待确认。

图标中的商标与标志归各自所有者。

- 文件名带 `-color` 的是彩色图标，按原色绘制；PNG 也算彩色。彩色图标由 `src/icons.rs` 按显示尺寸的实际像素栅格化
  （GPUI 自己画 SVG 时只按 `1em` = 12px 的两倍出位图，再拉大会发虚）。
- 其余是单色图标（`fill="currentColor"`），按当前文字颜色着色，深浅色模式下都可见。

## 改过和另找的几个

- `openclaw-color.svg`：来自 [BlueOcean223/cc-switch](https://github.com/BlueOcean223/cc-switch)（MIT）的
  `src/icons/extracted/claw.svg`，那边自己画的龙虾钳，不是 OpenClaw 官方的标志。
- `hermes.png`：magpie 的 `hermes.svg` 是个「H」字形，不是 Hermes 的标志（用户 10-11 指出）。
  换成 cc-switch 的 `hermes.png` 原图：Hermes Agent 的吉祥物头像，256px，白底黑线、圆角。按原色画、不跟系统色走，
  深色模式下就是一块白底的贴纸。之前试过描成单色路径，深色模式整张反相，也试过反相成深底白线，都不对
  （用户 10-11：照别人的做法保留原图）。
- `codex-color.svg`：删掉了第一条 `fill="#fff"` 的圆角方块路径。原件带白色底板，放在卡片上像套了一层白框。
- `claudecode-color.svg`：原件只有一条 `evenodd` 路径，两只眼睛是镂空的，露出底色；补了一条 `#141413` 的路径把眼睛填黑
  （Clawd 的眼睛是黑的，用户 10-11 指出）。
