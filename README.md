<div>
<img src="assets/images/logo-white.svg" alt="LEDTransit Logo" height="32" align="right"/>
<h1>Firmware for the LEDTransit Maps</h1>
</div>

This repository contains the firmware running on the LEDTransit maps &mdash; a series of pixel-based public transport maps designed and manufactured by [LEDTransit](https://ledtransit.com/).

The maps are powered by an ESP32-C3 (RISC-V) microcontroller and feature a custom PCB with an on-board JTAG USB interface for programming and debugging, as well as an LED chain for displaying the position of public transport vehicles in real-time.

The firmware is written in Rust using the `esp-hal` and `embassy` no_std/async frameworks and can be built from source and installed on the device by following the instructions below.

<div class="grid" markdown>

<img src="assets/images/bln1-2512-1.jpeg" alt="LEDTransit Map" width="400"/>
<img src="assets/images/bln1-2512-1-close.jpeg" alt="LEDTransit Map Close-up" width="400"/>

</div>

## Toolchain installation

Required tools: Rust toolchain with the `riscv32imc` target, `probe-rs` dev tools, `espflash` flasher, `protoc` protobuf compiler.

<details open>
<summary>macOS Homebrew</summary>

```sh
brew install protobuf
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup toolchain install stable --component rust-src
rustup target add riscv32imc-unknown-none-elf
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/probe-rs/probe-rs/releases/latest/download/probe-rs-tools-installer.sh | sh
cargo install espflash --locked
```

</details>

<details>
<summary>Linux Ubuntu</summary>

```sh
apt update && apt install -y curl build-essential protobuf-compiler ca-certificates
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup toolchain install stable --component rust-src
rustup target add riscv32imc-unknown-none-elf
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/probe-rs/probe-rs/releases/latest/download/probe-rs-tools-installer.sh | sh
cargo install espflash --locked
```

A working Linux Ubuntu x64 

</details>

Check that the tools are correctly installed using the `cargo xtask doctor` command:

```sh
cargo xtask doctor
```

For IDE integration, you may also want to install the [rust-analyzer](https://github.com/rust-lang/rust-analyzer) extension. Set the `PROTOC` environment variable to the path of the `protoc` binary and configure the default build target to `riscv32imc-unknown-none-elf` (for an example see [.vscode/settings.json](.vscode/settings.json)).

## Build the firmware

Build the firmware for a specific product using the `cargo xtask build` command ([see Supported Products](#supported-products)). If no product is specified, the build command will attempt to auto-detect a connected device and build for that product.

```sh
cargo xtask build # <product>
# Example: cargo xtask build bln1-2512-1
```

## Connect the device

- Connect a LEDTransit map to your computer using a USB-C data cable.
- Allow the USB device to connect when prompted.
- A JTAG/serial device should appear among your USB devices:

<details open>
<summary>macOS 26+</summary>

```sh
$ system_profiler SPUSBHostDataType
>   USB JTAG/serial debug unit:
>        Location ID: 0x00140000
>        Connection Type: Removable
>        ...
```

</details>

<details>
<summary>Linux</summary>

```sh
$ lsusb | grep JTAG
Bus 001 Device 001: ID 303a:1001 303a USB JTAG/serial debug unit  Serial: DC:06:32:B7:CD:29
```

</details>

A standard 500 mA USB 2.0 port is sufficient to power the device for small brightness levels.

## Flash the firmware

Program the device using the `cargo xtask run` command with the appropriate product identifier. If no product is specified, the command will attempt to auto-detect a connected device and flash the corresponding firmware.

```sh
cargo xtask run # <product>
```

> [!NOTE]
> When the log output of the device is monitored using the `probe-rs` tool invoked by `cargo xtask monitor` or `cargo xtask run`, the LEDs will flicker due to timing issues. This is expected behavior and can be resolved by detaching the RTT logger with Ctrl-C after the firmware has booted and the log output is no longer needed.

> [!WARNING]
> Program the device at your own risk. Burning E-Fuses is irreversible and may permanently make the device unusable for the application.
> Exceeding the maximum current rating of the board may trip the resettable on-board fuse.
> Exceeding the maximum current rating of your computer's USB port may disconnect the device or damage your computer.

## Other commands

- `cargo xtask monitor` - Monitor the device's serial output using `probe-rs`. Decoding the log output only works if the firmware installed on the device is exactly identical to the firmware built by your local source build (`./target/riscv32imc-unknown-none-elf/release/fw-ledtransit-map`). Meaning that you have to build and flash the firmware once using `cargo xtask run` before you can monitor the log output.

## Recovery: Bootloader mode

If the device is in a state where it cannot be programmed (e.g. the USB interface was re-configured from within the firmware or the LEDs exceed the USB port's current limit and trigger a device eject), you can force the device into bootloader mode by holding the middle button (circle icon) on the back side while plugging in the USB cable.
The application firmware will not boot in this mode, allowing you to re-flash the device using the `cargo xtask run` command.
After flashing, detach and re-attach the USB cable without holding the button to boot into the application firmware once again.

## Supported Products

| Product     | Model                             | Year | Status                                                                                     | MCU      |
| ----------- | --------------------------------- | ---- | ------------------------------------------------------------------------------------------ | -------- |
| bln2-2512-1 | Berlin Rapid Transit Lightmap XL  | 2026 | [Available for purchase](https://ledtransit.com/products/berlin-s-und-u-bahn-livekarte-xl) | ESP32-C3 |
| bln1-2512-1 | Berlin Rapid Transit Lightmap     | 2025 | [Available for purchase](https://ledtransit.com/products/berlin-s-und-u-bahn-livekarte)    | ESP32-C3 |
| bln1-2412-1 | Berlin Rapid Transit Lightmap Dev | 2024 | Internal                                                                               | ESP32-C3 |

## Contributing

Contributions to the firmware are welcome! Please open an issue or submit a pull request if you have any suggestions or improvements.
Please note that we will not consider fully LLM-generated code contributions, as we want to ensure that all code remains maintainable.

## License

The firmware is licensed under the [GPL-3.0](https://www.gnu.org/licenses/gpl-3.0.en.html) license. See the [LICENSE](LICENSE) file for details.
Meaning that you are free to use, modify, and distribute the firmware, but any derivative works must also be released open-source under the same license.

## Project structure

```text
├── assets            : Static files
│   ├── boot_image    : Bootloader binary image built from /bootloader
│   ├── certs         : TLS certificate bundle
│   ├── docker        : Dockerfiles for automated CI builds
│   ├── images        : Images embedded in markdown documentation
│   ├── prod_config   : Product configuration files (auto-generated)
│   ├── proto_schema  : Protobuf schema file to generate Rust WS-proto sources
│   └── prov_public   : Minified provisioning server UI built from /prov_server
│   ├── secure_ota    : Public key to verify signed OTA updates
├── bootloader        : Custom ESP-IDF bootloader source code (C)
├── prov_server       : Provisioning server source code (Rust)
├── src               : Firmware source code (Rust)
└── xtask             : Custom build and utility tasks (Rust)
```
