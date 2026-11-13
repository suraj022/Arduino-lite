# Arduino Lite

Arduino Lite is a lightweight desktop IDE for Arduino-compatible
development boards. It provides a GTK4/libadwaita interface around
[`arduino-cli`](https://arduino.github.io/arduino-cli/), with a
GtkSourceView code editor and tools for building, uploading, and
monitoring sketches.

The application delegates board discovery, compilation, uploads, and
serial communication to `arduino-cli`. Long-running operations run in
background threads so the interface remains responsive.

## Features

-   Syntax-highlighted code editor powered by GtkSourceView
-   Create, open, edit, save, and save-as sketch files
-   Multi-file project tabs for common source files (`.ino`, `.cpp`,
    `.c`, `.h`, and `.hpp`)
-   Verify/compile sketches using `arduino-cli`
-   Upload compiled sketches to a connected board
-   Board selection and board-specific menu options
-   Board Manager for installing and managing Arduino platforms/cores
-   Serial port discovery and serial monitor
-   Configurable serial baud rate
-   Build output and operation logs
-   Remembers application settings and the previously selected board

## Requirements

### Runtime requirements

-   Linux desktop with GTK 4
-   libadwaita
-   GtkSourceView 5
-   [`arduino-cli`](https://arduino.github.io/arduino-cli/)

### Build requirements

-   Rust toolchain (`rustc` and `cargo`)
-   `pkg-config`
-   GTK 4 development files
-   libadwaita development files
-   GtkSourceView 5 development files

On Ubuntu, install the system dependencies with:

``` bash
sudo apt update
sudo apt install \
    build-essential \
    pkg-config \
    libgtk-4-dev \
    libadwaita-1-dev \
    libgtksourceview-5-dev
```

Install Rust using [rustup](https://rustup.rs/) if it is not already
installed:

``` bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
```

Install `arduino-cli` using the [official installation
instructions](https://arduino.github.io/arduino-cli/installation/).
Confirm that it is available:

``` bash
arduino-cli version
```

If the executable is not named `arduino-cli` or is not in your `PATH`,
set `ARDUINO_CLI` to its full path before launching the application:

``` bash
export ARDUINO_CLI="/full/path/to/arduino-cli"
```

## Build and run from source

Clone the repository and enter its directory:

``` bash
git clone <REPOSITORY_URL>
cd <REPOSITORY_DIRECTORY>
```

Build the application in release mode:

``` bash
cargo build --release
```

Run it directly:

``` bash
cargo run --release
```

The compiled executable is normally created at:

``` text
target/release/<BINARY_NAME>
```

Replace the placeholders above with the actual repository URL,
directory, and Cargo binary name.

## First-time setup

1.  Launch Arduino Lite.

2.  Open **Board Manager** and install the platform/core for your board.
    Alternatively, install a core from the terminal. For example, for
    classic AVR boards:

    ``` bash
    arduino-cli core update-index
    arduino-cli core install arduino:avr
    ```

3.  Connect your board over USB.

4.  Select the board and detected serial port in the application.

5.  Open or create a sketch.

6.  Choose **Verify** to compile, or **Upload** to compile and upload to
    the selected board.

The AVR core command is only an example. Install the core that matches
your board. Other platforms use different core identifiers; consult the
board manufacturer's documentation or the Arduino CLI platform index.

## Serial port permissions on Linux

If the application can see a serial device but cannot open it, your
Linux user may need permission to access the serial port. On Ubuntu,
many boards expose ports owned by the `dialout` group.

Check the device permissions:

``` bash
ls -l /dev/ttyACM0
```

If the device belongs to `dialout`, add your user to that group:

``` bash
sudo usermod -aG dialout "$USER"
```

Log out and log back in for the group change to take effect. The device
name may be `/dev/ttyUSB0` or something else depending on the board and
USB adapter.

## Configuration

-   `ARDUINO_CLI`: Optional path to the `arduino-cli` executable. If
    unset, the application searches for `arduino-cli` in `PATH`.
-   Board selection, editor preferences, and other application settings
    are stored in the user's application configuration directory.

## Troubleshooting

### `arduino-cli` not found

Install Arduino CLI and make sure `arduino-cli version` works in a
terminal. If it is installed in a non-standard location, set
`ARDUINO_CLI` to its full executable path.

### No boards appear in the board selector

Update the Arduino CLI package index and install the appropriate
platform/core:

``` bash
arduino-cli core update-index
arduino-cli core search
```

Then install the core for your board and refresh the board list in the
application.

### No serial port appears

-   Check the USB cable; some cables provide power only.

-   Check that the board is detected by Linux:

    ``` bash
    ls /dev/ttyACM* /dev/ttyUSB* 2>/dev/null
    ```

-   Check user permissions as described above.

-   Close other programs that may already be using the port.

### Compilation or upload fails

Check the output panel for the `arduino-cli` error. Confirm that the
selected board is correct, its core is installed, and the required
libraries are available. For upload failures, also check the selected
port, USB connection, and bootloader/reset behavior of the board.

## Project status

This README describes the current source code's functionality and a
source-build workflow. A packaged release, installer, or
distribution-specific package should be documented separately once one
is available.

## Contributing

Bug reports, fixes, and improvements are welcome. When reporting a
problem, include:

-   Linux distribution and version
-   Rust version (`rustc --version`)
-   Arduino CLI version (`arduino-cli version`)
-   Board model and selected port, if relevant
-   The complete error output from the application

## License

Add the project's chosen license here before distributing it. Until a
license is included, do not assume that the source is available for
unrestricted reuse.
