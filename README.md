# System Recorder

`System Recorder` 是一个基于 Rust 开发的轻量级 Windows 音频录制工具。它允许用户灵活地捕获来自不同源的音频流并将其保存为标准 WAV 文件。

本项目通过直接调用 Windows 核心音频 API (**WASAPI**) 实现，旨在提供高性能、低延迟的音频采集体验。

## 🚀 核心功能

### 1. 多样化的录音模式

- **麦克风 (Mic)**：捕获物理输入设备（如外部麦克风、笔记本内置麦克风）的声音。
- **系统 Loopback**：捕获当前输出端点（扬声器/耳机）的所有音频流，实现“听到什么录什么”。
- **特定应用捕获 (Per-App)**：**（高级功能）** 仅录制某个特定进程的音频。此功能依赖于 Windows 10 build 20348+ 或 Windows 11 的新特性。

### 2. 实时设备枚举

- 自动检测并列出所有输入和输出音频端点。
- 实时显示当前系统中活跃的音频会话（Audio Sessions），方便用户快速定位需要录制的应用程序及 PID。

### 3. 直观的操作界面

- 基于 `egui` 构建的即时模式 (Immediate Mode) GUI。
- 支持原生 CJK 字体加载，确保在中文 Windows 环境下无乱码显示。
- 提供实时录制计时器和结果状态反馈。

## 🛠️ 技术实现

### WASAPI 与 COM Apartment 管理

本项目最核心的技术挑战在于处理 Windows 的 **COM 线程模型**：

- `eframe`/`winit` 在 Windows 上会将主线程初始化为 **STA (Single-Threaded Apartment)**。
- 然而，WASAPI 的某些关键接口（如 `ActivateAudioInterfaceAsync`）要求在 **MTA (Multi-Threaded Apartment)** 环境下运行。

为了解决这一冲突，本项目实现了一个专用的 **MTA 工作线程 (`run_on_mta`)**：

- 所有的 WASAPI 操作都会被分发（Dispatch）到这个后台线程中执行。
- 后台线程在启动时调用 `CoInitializeEx(None, COINIT_MULTITHREADED)`，确保所有 COM 调用均在 MTA 环境下完成。
- 使用同步通道 (Channel) 将结果返回给 GUI 线程进行展示。

### 音频处理流水线

- **捕获**：通过 WASAPI 的 Loopback 接口获取原始 PCM 数据。
- **存储**：利用 `hound` 库将采集到的采样数据实时写入 WAV 文件，无需在内存中缓存整个录音片段。

## 💻 系统要求

- **操作系统**：Windows 10 或 Windows 11 (仅支持 Windows)。
- **Per-App 捕获要求**：需 Windows 10 Build 20348 或更高版本。
- **开发环境**：Rust 1.75+ (Edition 2024)。

## 📖 快速开始

### 编译安装

确保您已安装 Rust 工具链及其对应的 Windows SDK。

```bash
# 克隆项目
git clone https://github.com/fernando42/systemRecorder.git
cd systemRecorder

# 编译发布版本 (优化性能，减少音频卡顿)
cargo build --release
```

### 使用指南

1. **运行程序**：启动生成的 `.exe` 文件。
2. **选择源**：在底部的录制面板中选择 `麦克风`、`系统 loopback` 或 `特定应用`。
3. **选择设备/进程**：
   - 麦克风 $\rightarrow$ 在下拉列表中选择对应的输入设备。
   - 系统 Loopback $\rightarrow$ 选择输出端点（如扬声器）。
   - 特定应用 $\rightarrow$ 从“音频会话”列中选择目标应用。
4. **设置路径**：指定 WAV 文件的保存位置及文件名。
5. **开始录制**：点击 `● 开始录制`，此时界面将显示实时时长。
6. **停止并保存**：点击 `■ 停止`，程序将自动关闭文件句柄并完成写入。

## 📦 依赖项

- `eframe`/`egui`: 图形用户界面。
- `windows`: Windows API 绑定。
- `hound`: WAV 文件编码。
- `anyhow`/`thiserror`: 错误处理。
