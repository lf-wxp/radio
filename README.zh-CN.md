# ESP-Radio

> 一个面向 **ESP32-C6** 的完整 FM 收音机固件，纯 Rust 编写。
> 通过 SoftAP 强制门户配网，旋转编码器交互，配 240×320 ST7789 屏幕上的 Material 风格 Slint UI。

[English](./README.md) · [简体中文](./README.zh-CN.md)

<p>
  <img alt="rust"        src="https://img.shields.io/badge/rust-nightly-orange?logo=rust">
  <img alt="target"      src="https://img.shields.io/badge/target-riscv32imac--unknown--none--elf-blue">
  <img alt="chip"        src="https://img.shields.io/badge/chip-ESP32--C6-red?logo=espressif">
  <img alt="esp-hal"     src="https://img.shields.io/badge/esp--hal-1.1-success">
  <img alt="esp-radio"   src="https://img.shields.io/badge/esp--radio-0.18-success">
  <img alt="slint"       src="https://img.shields.io/badge/slint-1.16-7c3aed?logo=slint">
  <img alt="embassy"     src="https://img.shields.io/badge/runtime-embassy-2dd4bf">
  <img alt="no_std"      src="https://img.shields.io/badge/no__std-yes-informational">
</p>

---

## 📑 目录

- [ESP-Radio](#esp-radio)
  - [📑 目录](#-目录)
  - [✨ 功能特性](#-功能特性)
  - [🎬 演示](#-演示)
  - [🧱 硬件接线](#-硬件接线)
  - [🏗️ 架构](#️-架构)
  - [🚦 启动时序](#-启动时序)
  - [🧩 模块概览](#-模块概览)
  - [📁 项目结构](#-项目结构)
  - [🚀 快速开始](#-快速开始)
    - [1. 工具链](#1-工具链)
    - [2. 编译并烧录](#2-编译并烧录)
    - [3. WiFi 配网（仅首次）](#3-wifi-配网仅首次)
  - [🛠️ `cargo make` 任务一览](#️-cargo-make-任务一览)
  - [🖥️ 宿主机 UI 预览](#️-宿主机-ui-预览)
    - [macOS 26（Tahoe）注意](#macos-26tahoe注意)
  - [📡 RDS 能力矩阵](#-rds-能力矩阵)
  - [📦 性能与体积](#-性能与体积)
  - [🔄 开发工作流](#-开发工作流)
  - [🧰 技术栈](#-技术栈)
  - [🌐 Web 控制台](#-web-控制台)
    - [REST API](#rest-api)
    - [向设备推送固件升级（OTA）](#向设备推送固件升级ota)
  - [🐛 常见问题与 FAQ](#-常见问题与-faq)
  - [🗺️ 路线图](#️-路线图)
    - [✅ 已完成](#-已完成)
    - [🚧 规划中（不增加任何硬件）](#-规划中不增加任何硬件)
    - [🚫 当前硬件下不做（透明列出）](#-当前硬件下不做透明列出)
    - [📑 设计文档](#-设计文档)
  - [🤝 参与贡献](#-参与贡献)
  - [🙏 致谢](#-致谢)
  - [📜 License](#-license)

---

## ✨ 功能特性

- 📻 **FM 调谐** —— Si4703 通过 I²C 驱动，开机自动扫描并跳到信号最强的电台；UI 支持 RDS（PS 电台名 + RT 滚动文字，兼容 GB2312/UTF-8 扩展）。
- 🔊 **音量与静音** —— 超长按编码器（≥ 2.5 s）可静音，UI 顶部带独立音量条。
- 🎛️ **触感操作** —— KY-040 旋转编码器接入 ESP32-C6 的 **PCNT** 硬件外设，无中断抖动；旋转调台（带加速），短按 = 循环收藏台，长按（≥ 800 ms）= 保存当前频点。
- ⭐ **Presets 收藏台 + 上电恢复** —— 可保存 8 个收藏台到 Flash（`esp-storage`，v2 表体同时缓存每个收藏台的 RDS PI 与 PS，Web 控制台上预置按钮会显示 `BBC R1` 而不是贤果 `97.7`）；下次开机自动调回上一次听的频率（带 30 s 防抖，避免 Flash 费压）。
- 📶 **WiFi 配网** —— 首次开机弹出 SoftAP 强制门户页面，凭据由 `esp-storage` 写入 Flash，下次开机自动联网。
- 🖥️ **Slint UI** —— 基于 Material-1.0 主题的 [`ui/radio_ui.slint`](./ui/radio_ui.slint)，软件渲染到 240×320 ST7789，可在 macOS / Linux / Windows 上离线预览。
- 🔁 **Embassy 异步** —— 全程 `no_std`，`embassy-executor` + `embassy-sync` 在输入任务、电台控制任务、UI 渲染循环之间传递事件。
- 🛠️ **`cargo make` 一站式工作流** —— 编译、烧录、Lint、固件体积分析、宿主机 UI 预览全部一条命令搞定。
- 🧪 **设备端测试** —— `embedded-test` + `probe-rs`，单元测试直接跑在真机上。
- 🩺 **自诊断** —— 开机自检（POST）验证 I²C 总线、Si4703 芯片 ID、堆分配器、PCNT 编码器；运行时软件看门狗监控电台控制任务存活状态。两者均通过 `GET /api/health` 暴露。

---

## 🎬 演示

| 启动画面 | 调谐+RDS | 静音 / 音量 |
|:---:|:---:|:---:|
| _补充 `docs/screenshots/boot.png`_ | _补充 `docs/screenshots/rds.png`_ | _补充 `docs/screenshots/mute.png`_ |

> 💡 不需要硬件也可以预览同一份 UI：
> `cargo make ui-preview-data` —— 详见 [宿主机 UI 预览](#%EF%B8%8F-宿主机-ui-预览)。

---

## 🧱 硬件接线

| 功能              | ESP32-C6 GPIO |
|-------------------|---------------|
| ST7789 SCK        | GPIO3         |
| ST7789 MOSI       | GPIO0         |
| ST7789 CS         | GPIO1         |
| ST7789 DC         | GPIO2         |
| ST7789 RST        | GPIO22        |
| ST7789 BLK 背光   | GPIO23        |
| Si4703 SDA (SDIO) | GPIO6         |
| Si4703 SCL (SCLK) | GPIO7         |
| Si4703 RST        | GPIO10        |
| 编码器 S1 (CLK)   | GPIO11        |
| 编码器 S2 (DT)    | GPIO18        |
| 编码器 KEY        | GPIO19        |

**用户交互**

- 旋转编码器 → 按 0.1 MHz 步进调台。
- 短按按键 → 循环切到下一个收藏台（未保存任何频点时退化为自动搜台）。
- 长按按键（≥ 800 ms）→ 将当前频点保存到下一个收藏槽（8 个槽顺序覆盖）。
- 超长按（≥ 2.5 s）→ 切换静音。

---

## 🏗️ 架构

三个并发的 embassy 任务，通过无锁通道与一把共享互斥锁解耦：

```mermaid
flowchart LR
    subgraph HW [硬件]
      ENC[KY-040 编码器<br/>+ PCNT]
      TUNER[Si4703 调谐器<br/>I²C]
      LCD[ST7789 屏幕<br/>SPI]
    end

    subgraph TASKS [Embassy 任务]
      IN[input_task]
      RADIO[radio_task]
      UI[ui_task]
    end

    ENC -- 脉冲 + 按键 --> IN
    IN  -- InputCmd<br/>Channel 8 --> RADIO
    RADIO <-- I²C --> TUNER
    RADIO -- RadioState<br/>Mutex --> UI
    UI  -- 帧缓冲 --> LCD
```

- **`input_task`** 处理编码器脉冲与按键去抖，发送 `InputCmd::{TuneDelta, Seek, ToggleMute}` 事件。
- **`radio_task`** 拥有 Si4703 驱动，执行调台/搜台/静音、解析 RDS，将快照写入互斥保护的 `RadioState`。
- **`ui_task`** 大约 30 fps 唤醒，读取最新 `RadioState`，刷新绑定到 [`ui/radio_ui.slint`](./ui/radio_ui.slint) 的 Slint 模型。

---

## 🚦 启动时序

```mermaid
sequenceDiagram
    autonumber
    participant Boot as main()
    participant HW as hardware::init
    participant Wifi as WifiProvisioner
    participant Disp as Display + Slint
    participant Tuner as Si4703
    participant Tasks as embassy::Spawner

    Boot->>HW: 配置时钟 / SPI / I²C / PCNT
    HW-->>Boot: 引脚 + 外设
    Boot->>Disp: 初始化 ST7789 + Slint 平台
    Disp-->>Boot: 窗口 + 帧缓冲
    Boot->>Wifi: 从 Flash 读取凭据
    alt 已有凭据
        Wifi-->>Boot: STA 模式连接
    else 无凭据
        Wifi-->>Boot: 启动 SoftAP 配网门户
        Note over Wifi: 用户提交 SSID/密码<br/>凭据持久化到 Flash
    end
    Boot->>Tuner: 上电 + 自动搜索最强电台
    Tuner-->>Boot: 初始频率
    Boot->>Tasks: spawn(input_task, radio_task, ui_task)
    Tasks-->>Boot: 运行中 ⏱️
```

---

## 🧩 模块概览

可复用部分以 `no_std` 库形式提供，方便兄弟固件直接引用。

| 模块 | 文件 | 职责 |
|---|---|---|
| `display`         | [src/display/mod.rs](./src/display/mod.rs)               | ST7789 SPI 驱动、双帧缓冲、Slint 平台对接（`Platform` / `WindowAdapter` / `LineBufferProvider`）。 |
| `rotary_encoder`  | [src/rotary_encoder/mod.rs](./src/rotary_encoder/mod.rs) | 基于 **PCNT** 外设的 KY-040 驱动，自带溢出处理，输出干净的 `±N` 增量与按键事件，无中断抖动。 |
| `si4703`          | [src/si4703/mod.rs](./src/si4703/mod.rs)                 | Si4703 I²C 寄存器映射、调台/搜台/音量/静音、RDS A/B 组解码（PS、RT、PI），预留 GB2312/UTF-8 扩展钩子。 |
| `wifi_provision`  | [src/wifi_provision/mod.rs](./src/wifi_provision/mod.rs) | 基于 `picoserve` 的 SoftAP + DHCP + DNS 重定向门户，配套 [`storage.rs`](./src/wifi_provision/storage.rs) 实现 Flash 持久化。 |

---

## 📁 项目结构

```text
esp-radio/
├── src/
│   ├── lib.rs                    # 可复用的驱动 crate（no_std）
│   ├── display/                  # ST7789 SPI 驱动 + Slint 平台对接
│   ├── rotary_encoder/           # KY-040 编码器驱动（基于 PCNT 外设）
│   ├── si4703/                   # Si4703 FM 芯片 I²C 驱动 + RDS 解析
│   ├── wifi_provision/           # SoftAP 配网门户 + Flash 持久化
│   └── bin/radio/                # 主固件（拆分为 5 个模块）
│       ├── main.rs               # 启动流程与装配
│       ├── hardware.rs           # GPIO/SPI/I²C/PCNT 初始化
│       ├── state.rs              # 共享 embassy-sync 原语
│       ├── tasks.rs              # 异步任务（输入 / 电台 / UI）
│       ├── ui.rs                 # Slint 与电台状态的桥接
│       ├── diagnostics.rs        # 开机自检（POST）+ 软件看门狗
│       └── web.rs                # picoserve HTTP 服务 + REST API
├── ui/
│   ├── radio_ui.slint            # 主 Material UI
│   ├── preview_data.json         # 宿主机预览样例数据
│   ├── main.slint
│   └── slint_st7789_ui.slint
├── examples/                     # 单功能示例
│   ├── si4703_fm_radio.rs
│   ├── rotary_encoder.rs
│   ├── slint_st7789.rs
│   └── wifi_provision.rs
├── material-1.0/                 # 内置的 Slint Material 组件库
├── Cargo.toml
├── Makefile.toml                 # cargo-make 任务集
├── rust-toolchain.toml           # nightly + riscv32imac 目标
└── build.rs
```

---

## 🚀 快速开始

### 1. 工具链

[`rust-toolchain.toml`](./rust-toolchain.toml) 已经锁定通道与 target，`rustup` 会自动下载：

```bash
# 必备 cargo 工具
cargo install cargo-make
cargo install probe-rs --features cli   # cargo run / probe-rs attach 都依赖它

# 可选：宿主机 UI 预览（cargo-make 也会自动安装）
cargo install slint-viewer
```

### 2. 编译并烧录

```bash
# 通过 USB 接好 ESP32-C6 后：
cargo make flash-release          # 编译并烧录主固件
cargo make monitor                # 实时查看 RTT 日志

# 或者只跑某个示例：
cargo make flash-example -e EXAMPLE=si4703_fm_radio
```

### 3. WiFi 配网（仅首次）

1. 烧录完成后，设备会启动名为 `ESP-Radio-Setup` 的 **SoftAP**。
2. 用手机/电脑连接，强制门户页会自动弹出。
3. 选择家里的 SSID 并输入密码，凭据写入 Flash 后设备重启进入 STA 模式。

---

## 🛠️ `cargo make` 任务一览

| 任务                        | 用途                                                          |
|-----------------------------|----------------------------------------------------------------|
| `build` / `build-release`   | 编译主固件                                                    |
| `build-all` / `…-release`   | 编译库 + 所有示例                                             |
| `build-example`             | 编译单个示例（`EXAMPLE=<名字>`）                              |
| `flash` / `flash-release`   | 编译 **并** 通过 `probe-rs run` 烧录                          |
| `flash-example`             | 烧录指定示例（`EXAMPLE=<名字>`）                              |
| `monitor`                   | 附加 `probe-rs` 输出 defmt 日志                               |
| `check` / `clippy` / `fmt`  | 标准代码质量检查                                              |
| `fmt-check`                 | 仅检查格式不修改文件                                          |
| `size` / `size-example`     | 用 `rust-size` 输出 release 固件大小                          |
| `test`                      | 设备端测试（`embedded-test` + `probe-rs`）                    |
| `host-test`                 | 宿主侧纯逻辑单元测试（不依赖硬件）                            |
| `clean`                     | `cargo clean`                                                 |
| `ci`                        | `fmt-check` + `clippy` + `host-test` + `build-all-release`    |
| `dev`                       | 快速开发循环：`check` + `clippy`                              |
| `release`                   | 完整发布流水线                                                |
| `ui-install-viewer`         | 安装 / 校验宿主机的 `slint-viewer`                            |
| `ui-preview`                | 在宿主机实时预览 UI（保存自动刷新）                           |
| `ui-preview-data`           | 同上，但预先加载 RDS / 音量样例数据                            |
| `ota-image`                 | 调用 `espflash save-image` 组装一个可供 OTA 下发的 `radio.bin`     |
| `ota-serve`                 | 依赖上一步，启动 Rust 开发服务器并打印 LAN URL 与二维码             |

---

## 🖥️ 宿主机 UI 预览

无需连接 ESP32 即可迭代 Slint UI：

```bash
cargo make ui-preview-data
```

会弹出原生窗口并加载 [`ui/preview_data.json`](./ui/preview_data.json) 的模拟数据，编辑 [`ui/radio_ui.slint`](./ui/radio_ui.slint) 保存即热更新。

### macOS 26（Tahoe）注意

部分传递依赖（如 `bonjour-sys`）会直接调用 `bindgen`，在 macOS 26 SDK 下会报 `architecture not supported`。本仓库的 `ui-*` 任务已经自动注入 `SDKROOT` 和 `BINDGEN_EXTRA_CLANG_ARGS`，**直接 `cargo make ui-preview` 即可**，无需手动 export 任何环境变量。

---

## 📡 RDS 能力矩阵

| 能力                            | 状态  | 说明 |
|---------------------------------|:-----:|------|
| 节目识别码 (PI)                 | ✅    | 用作电台指纹缓存。 |
| 节目名 (PS, 8 字符)             | ✅    | 显示为加粗的电台名。 |
| 节目文本 (RT, ≤ 64 字符)        | ✅    | UI 滚动展示。 |
| GB2312（中文扩展）              | ✅    | 检测到帧头后自动回退到 UTF-8。 |
| UTF-8（RDS 扩展）               | ✅    | 通过引导序列自动识别。 |
| 交通通告 (TA)                   | 🟡    | 已解码，UI 暂未呈现。 |
| 时间码 (CT)                     | ✅    | 解码自 group 4A，顶栏以 `HH:MM` 呈现（已叠加本地时区偏移）。 |
| 备用频率 (RDS-AF)               | ✅    | Group 0A block C 已解析；RSSI 连续 5 s ≤18 时探测并切换最强 AF，PI 不匹配自动回滚。 |
| RadioText Plus (RT+, AID 0x4BD7)| ✅    | Group 3A 注册 ODA、group 11A 承载标签；广播时 "now playing" chip 显示 `{歌手} — {歌名}`。 |

✅ 已上线 · 🟡 部分支持 · ⏳ 规划中

---

## 📦 性能与体积

来自 `cargo make build-release` 在 Rust nightly 下（LTO `fat`、`opt-level=z`）的典型数据：

| 指标                            | 典型值                       |
|--------------------------------|------------------------------|
| Flash 镜像（`.text`）          | ~ 740 KB                     |
| 静态 RAM（`.bss` + `.data`）   | ~ 90 KB                      |
| 堆（`esp-alloc`）              | 96 KB 预留                   |
| Slint 帧缓冲                   | 1 行 × 240 × 16 bpp          |
| UI 渲染帧率                    | ~ 30 fps                     |
| 调台 → 出声延迟                | < 120 ms                     |
| 上电到首帧                     | ~ 850 ms（凭据已缓存时）     |

> 在 release 构建后运行 `cargo make size` 可在 **你的** 工具链下打印精确的段大小。

---

## 🔄 开发工作流

```mermaid
flowchart LR
    A([编辑代码]) --> B{cargo make}
    B -->|dev| C[check + clippy]
    B -->|ci|  D[fmt-check + clippy + host-test + build-all-release]
    B -->|release| E[fmt-check + clippy + host-test + build-all-release + size]
    C --> F[flash-release]
    D --> F
    E --> F
    F --> G[monitor — RTT defmt 日志]
    G --> A
```

**推荐迭代节奏**

1. 改 UI → `cargo make ui-preview-data`（即时热更新）。
2. 改驱动/逻辑 → `cargo make dev`（快速类型/Lint 检查）。
3. 上板调试 → `cargo make flash-release && cargo make monitor`。
4. 提 PR 前 → `cargo make ci`。

---

## 🧰 技术栈

- **MCU** —— ESP32-C6（RISC-V，单核，WiFi 6 + BLE 5）
- **异步运行时** —— [`embassy`](https://embassy.dev)（`embassy-executor` / `embassy-net` / `embassy-time` / `embassy-sync`）
- **HAL** —— [`esp-hal`](https://github.com/esp-rs/esp-hal) `1.1` + [`esp-rtos`](https://crates.io/crates/esp-rtos) `0.3`
- **WiFi/BLE** —— [`esp-radio`](https://crates.io/crates/esp-radio) `0.18`（coex + WiFi + BLE）
- **GUI** —— [`slint`](https://slint.dev) `1.16` 软件渲染，启用 `compat-1-2` + `unsafe-single-threaded`
- **显示驱动** —— [`mipidsi`](https://crates.io/crates/mipidsi) `0.10`（ST7789），通过 `embedded-hal-bus` 共享 SPI
- **存储** —— [`esp-storage`](https://crates.io/crates/esp-storage) 持久化 WiFi 凭据
- **日志** —— `defmt` + `rtt-target` + `panic-rtt-target`
- **构建** —— Rust nightly，`riscv32imac-unknown-none-elf`，`build-std=alloc,core`，LTO `fat`，`opt-level=z`

---

## 🌐 Web 控制台

WiFi 联上后，LCD 底部会显示 `http://<ip>`（就是路由器 DHCP
租约列表里那个地址）。在支持 mDNS 的设备上（macOS、iOS、装了
Avahi 的 Linux、开启“网络发现”的 Windows 10+、较新的 Android
Chrome）也可以直接打开 **<http://esp-radio.local/>**——同一个控
制台，免去查 IP 的麻烦。两种 URL 都能进入手机友好的遥控器界
面：频率大字、±0.1 MHz 按钮、直接输入跳转、收藏台快捷、实时
RDS PS / RT / AF / 时钟徽标。页面每秒拉取一次设备状态。

> 🔓 **零鉴权。** 必须在你信任的局域网使用，别把设备直接暴
> 露到公网 —— picoserve 自己的 README 也明确不推荐裸露公网。

### REST API

所有接口都在 80 端口，请求体为 JSON 或空。

| 方法   | 路径                  | 请求体                       | 作用                                                                       |
|--------|-----------------------|----------------------------|----------------------------------------------------------------------------|
| GET    | `/`                   | —                          | 单页 HTML 控制台。                                                          |
| GET    | `/api/state`          | —                          | JSON 状态快照：频率、RSSI、PS/RT/PTY/AF、静音、收藏台、WiFi。              |
| GET    | `/api/log`            | —                          | JSON 听音日志 — 最近 64 条采样（按时间顺序返回）。                            |
| GET    | `/api/health`         | —                          | JSON 健康快照：运行时长、堆内存、I²C 错误计数、看门狗状态、POST 结果。   |
| POST   | `/api/tune`           | `{"freq_x10":1015}`         | 调到 101.5 MHz；超出、87.5–108.0」返回 `400`。                              |
| POST   | `/api/tune/up`        | —                          | +0.1 MHz。                                                                  |
| POST   | `/api/tune/down`      | —                          | −0.1 MHz。                                                                  |
| POST   | `/api/preset/cycle`   | —                          | 跳到下一个收藏台（循环）。                                                  |
| POST   | `/api/preset/save`    | —                          | 保存当前频率（33 个槽全满后 FIFO 淘汰）。                                  |
| POST   | `/api/mute`           | —                          | 切换静音。                                                                  |
| POST   | `/api/ota`            | `{"url":"http://…/firmware.bin"}` | 从 URL 拉取新固件到空闲槽，校验合法后标记为下次启动。             |

所有命令走与旋钮完全相同的通道 —— web tune 与旋钮调台在控制任务内部自
然串行，未新增任何额外锁。

### 向设备推送固件升级（OTA）

开发期要把新版本推给一台已联网的设备，一条命令就够：

```bash
cargo make ota-serve
```

这条任务会依次串起三个阶段，带你从源码变更一路走到“设备能下载
的 URL”：

1. `build-release` —— 带优化编译固件。
2. `ota-image` —— 调用 `espflash save-image` 把 ELF 压成
   `target/…/release/radio.bin` 平面镜像。
3. `ota-serve` —— 启动仓库内的 Rust 开发服务器
   （`tools/ota-serve/`）监听 `0.0.0.0:8000`，在 `/firmware.bin`
   下提供镜像。终端会打印各 LAN 地址 + 二维码，手机扫一下
   把出来的 URL 贴进 Web 控制台 OTA 卡片，看进度条跑完即可。

这个服务器是一个独立的宿主机 crate，有意避开主项目面向
 RISC-V 的 `.cargo/config.toml`；cargo make 任务会重设
`RUSTUP_TOOLCHAIN` / `CARGO_BUILD_TARGET` / `RUSTFLAGS` 让主机
稳定工具链能照常编译。不需要 `python -m http.server`，也
不需要动 Cargo workspace，一句 `cargo make ota-serve` 到位。

---

## 🐛 常见问题与 FAQ

<details>
<summary><b>probe-rs 找不到芯片</b></summary>

检查 USB-JTAG 桥接是否连接、是否进入下载模式，重新 <code>cargo make monitor</code>。macOS 上还要确认 <code>系统设置 → 隐私 → USB</code> 已授权。
</details>

<details>
<summary><b>WiFi 一直连不上</b></summary>

**在开机启动画面期间**（收音机任务尚未启动时）长按编码器可清除已存凭据，下次开机会重新进入 SoftAP 配网门户。这个启动期手势和运行时的长按是两条独立路径——运行时长按只会保存当前频点为收藏台，不会动 WiFi 凭据。也可以用 <code>probe-rs erase --chip esp32c6</code> 直接擦除 Flash。
</details>

<details>
<summary><b>macOS 26（Tahoe）上 <code>bonjour-sys</code> 编译失败</b></summary>

请走 <code>cargo make ui-preview*</code>（已注入 <code>SDKROOT</code> 和 <code>BINDGEN_EXTRA_CLANG_ARGS</code>）。不要在 Tahoe 下裸跑 <code>cargo install slint-viewer</code>。
</details>

<details>
<summary><b>屏幕一直黑屏</b></summary>

确认 GPIO23 背光接好，并按 <code>SCK / MOSI / CS / DC / RST</code> 顺序核对 SPI 接线。最常见的坑是 <code>RST</code> 引脚悬空。
</details>

<details>
<summary><b>为什么在信号边缘的台上会短暂掉音？</b></summary>

这是 RDS-AF 跟随在探测备用频率。当信号连续 5 秒低于 RSSI
18 且广播台已告知 AF 列表（会在立体声指示器旁边看到
<code>AF·N</code> 徽标）时，收音机会依次 ping 每个备选频点比较
RSSI：PI 校验一致且信号明显提升则跳到最强频点，否则回滚到原
频率。探测后冷却 30 秒；未广播 AF 列表的台不会触发。
</details>

<details>
<summary><b>为什么必须用 nightly Rust？</b></summary>

我们依赖 <code>build-std</code> 在 <code>riscv32imac-unknown-none-elf</code> 下重建 <code>core</code>/<code>alloc</code>，并启用了若干仅 nightly 才支持的 <code>esp-hal</code> 特性。具体通道见 <a href="./rust-toolchain.toml"><code>rust-toolchain.toml</code></a>。
</details>

<details>
<summary><b>能跑在 ESP32 / ESP32-S3 / ESP32-C3 上吗？</b></summary>

大部分代码可移植，但当前 <code>Cargo.toml</code> 里 <code>esp-hal</code>、<code>esp-rtos</code>、<code>esp-radio</code>、<code>esp-storage</code> 的 feature 是写死 ESP32-C6 的。移植需要切换 feature 标志并重新核对 GPIO 表。
</details>

---

## 🗺️ 路线图

路线图分为三档，方便不同投入度的贡献者切入；每一档内按推荐实施顺序
自上而下排列。

### ✅ 已完成

- [x] FM 调谐 + 自动搜台 + RDS PS / RT
- [x] WiFi 强制门户配网 + Flash 持久化
- [x] ST7789 上的 Slint Material UI
- [x] 设备端测试（`embedded-test`）
- [x] RDS 时间码（CT）——从 group 4A 自动同步墙钟
- [x] RDS-AF 备用频率自动跟随——低信号探测 + PI 校验不一致回滚
- [x] 局域网 Web 控制台——手机友好单页 + JSON API（监听 80 端口）
- [x] mDNS 响应器——访问 `http://esp-radio.local/` 免去记 IP
- [x] 听音日志——内存环形缓冲存 PS / RT / RSSI，用于 Web 控制台回放
- [x] 开机自检（POST）——启动时验证 I²C 总线、芯片 ID、堆分配器、编码器，LCD 状态栏反馈结果
- [x] 软件看门狗——运行时监控电台控制任务存活状态（5 s 超时），通过 `/api/health` 暴露

### 🚧 规划中（不增加任何硬件）

下表中所有功能都基于现有外设落地：Si4703、ST7789、KY-040、Wi-Fi、
BLE 射频、Flash。工时为单人投入估算。

| #   | 功能                                                  | 工时    | 说明                                                                                  |
| --- | ----------------------------------------------------- | ------- | ------------------------------------------------------------------------------------- |
| ✅  | RDS PTY（节目类型）显示                              | 0.5 天  | block B 已在解码器中，一次位运算 + 32 项静态表即可。已于 2026-06 交付。              |
| ✅  | 立体声指示 + 弱信号自动 mono                         | 0.5 天  | 读取 Si4703 `STATUSRSSI` bit 8，复用现成 `set_mono`。tasks.rs 中只点递进。      |
| ✅  | RSSI 频谱图（“看图调台”）                            | 1 天    | 启动时一次 `sweep_rssi` 扫 87.5–108.0 MHz，52 格柱状图 + 当前频点高亮，LCD 与 Web 控制台两端都展示；Web 端新增 `GET /api/spectrum` 与 `Scan` 按钮（`POST /api/spectrum/scan`），可随时手动重新扫一遍。已于 2026-06 交付。 |
| ✅  | 旋钮调台加速                                          | 0.25 天 | 按 detent 间隔动态选档（×1/×2/×3/×5），反转或 idle 超时自动重置。已于 2026-06 交付。 |
| ✅  | 收藏台 Presets + 上电恢复上次频率                    | 1.5 天  | 写入 `storage` 分区（0x3E_0000）；短按循环收藏、长按保存、超长按静音。已于 2026-06 交付。v2 schema（2026-06）额外缓存 RDS PI + PS，Web 预置按钮优先显示 `BBC R1` 这类友好名；后台 metadata-fill 任务会为“保存时 RDS 还未锁”的槽位补上 PI/PS。Schema 是单向升级的 — 回滚到 v2 之前的固件会在下一次保存时抹除预置表。 |
| ✅  | RDS-AF 备用频率自动跟随                              | 2 天    | Group 0A block C 解析、PI 校验；RSSI 连续 5 s ≤ 18 且存在 AF 列表时探测并跳到最强频点，PI 不匹配则回滚。已于 2026-06 交付。 |
| ✅  | 局域网 Web 控制台（`/api/state`、`/api/tune`）       | 2 天    | 手机友好单页 HTML + JSON API，监听 80 端口；DHCP 完成后 LCD 底部显示访问 URL。已于 2026-06 交付。 |
| ✅  | mDNS 广播 `esp-radio.local`                          | 1 天    | 监听 `224.0.0.251:5353` 的被动 A 记录响应器，配合 #7 让用户能在手机上直接访问 `http://esp-radio.local/`。已于 2026-06 交付。 |
| ✅  | RDS 收听日志（PS/RT/RSSI 环形缓冲）                  | 1 天    | 64 条内存环形缓冲，每 10 秒采样一次；Web 控制台中"听音日志"面板呈现。Flash 持久化为保留 #11 的存储预算暂不实现。已于 2026-06 交付。 |
| ✅  | OTA 固件升级                                         | 3 天    | 全链路于 2026-06 交付：GPT 分区表、按扇区缓冲的 `OtaWriter`（复用 `esp-bootloader-esp-idf::OtaUpdater`）、HTTP 下载器、ESP 镜像头校验、Web 控制台触发（`POST /api/ota`）以及 LCD 上的全屏 Slint 进度浮层。`cargo make ota-serve` 启动内置 Rust 开发服务器并提供二维码；备忘在 [docs/ota-design.zh-CN.md](./docs/ota-design.zh-CN.md)。 |

### 🚫 当前硬件下不做（透明列出）

以下方向都需要新增硬件（音频 DAC、麦克风、触摸层、电量计 IC），
明确**不在**路线图内：

- 网络电台播放（HLS / Icecast）—— 需要 I²S DAC + 解码器。
- 经典蓝牙音频（A2DP source）—— 需要 Classic Bluetooth (BR/EDR) 射
  频，而 ESP32-C6 是 BLE-only 芯片，硬件就跑不起来；即使换到支持
  Classic 的 ESP32 系列，Rust 异步蓝牙生态（`esp-radio` /
  `trouble-host` / `bt-hci`）也只覆盖 BLE，要做 A2DP source 就得切
  回 ESP-IDF + Bluedroid (C)，并新增 I²S 音频 ADC 把 Si4703 模拟输
  出采回数字，工程量等于重做项目。
- BLE LE Audio source (Auracast / LC3) —— 芯片硬件支持，但 `esp-
  radio` 的 BLE controller 暂未暴露 Isochronous Channels，
  `crates.io` 上也没有 LC3 编码器。等上游协议栈和编码器到位后再评
  估。
- BLE HID 遥控 —— 已被 LAN Web 控制台（#7）覆盖：配对成本高、无屏
  上反馈、Web UI 都能做到的事 BLE 也做不出花来，不值得为它维护
  Wi-Fi/BLE coex。
- 实时音频 FFT 频谱 —— 板子上没有音频 ADC 路径。
- 触屏菜单 —— 当前 ST7789 模块无触摸层。
- 电池电量挂件 —— 参考板未引出电量计 IC。
- 睡眠定时器 / 闹钟 —— 唯一的时钟源是 RDS-CT，不广播 4A 组的台
  会让闹钟失效；当前使用场景下不值得为此引入额外的交互模式。

### 📑 设计文档

- [架构总览](./ARCHITECTURE.md) — 运行时拓扑、任务 / 通道图、启动序列、Flash 所有权。
- [OTA 固件升级 — 技术设计](./docs/ota-design.zh-CN.md)

---

## 🤝 参与贡献

欢迎贡献！提交 PR 之前请：

1. 本地跑一遍 `cargo make ci`，必须通过。
2. 一个 PR 只做一件事（一个功能或一个修复）。
3. PR 描述里写清楚动机，关联相关 issue。
4. [`src/lib.rs`](./src/lib.rs) 中新增公开 API 必须配 rustdoc 注释。
5. UI 改动请附上 `cargo make ui-preview-data` 的截图。

如果是较大改动，建议先开 issue 讨论方案。

---

## 🙏 致谢

没有这些社区，这个项目无从谈起：

- [esp-rs](https://github.com/esp-rs) —— `esp-hal`、`esp-rtos`、`esp-radio`、`esp-storage`、`esp-bootloader-esp-idf`。
- [embassy-rs](https://embassy.dev) —— 嵌入式异步运行时。
- [Slint](https://slint.dev) —— 嵌入式声明式 GUI。
- [mipidsi](https://github.com/almindor/mipidsi) —— 纯 Rust 的显示驱动。
- [probe-rs](https://probe.rs) —— 烧录、调试、RTT 日志。
- [Material Design](https://m3.material.io) 与本仓库内置的 [`material-1.0`](./material-1.0/) Slint 组件库。

---

## 📜 License

本仓库当前用于学习与原型用途。[`material-1.0/`](./material-1.0/) 内的 UI 资源沿用其上游 License（详见 `material-1.0/LICENSE.md`）。

项目源码：参见各文件头说明；若仓库尚未补充 LICENSE 文件，则在补充前作者保留所有权利。
