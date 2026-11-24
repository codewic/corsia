# Install and quick start

Corsia is early software. The steps below get you from nothing to a model served from more than one drive, and say at each step what is written to your disks.

## What you need

- **Linux**, kernel 5.6 or later (Corsia reads through io_uring).
- **Rust**, stable, 1.85 or later. Install it from [rustup.rs](https://rustup.rs) if `cargo --version` prints nothing.
- **fusermount3**, only for `corsia serve`. Most desktops have it. If `command -v fusermount3` prints nothing:

  ```bash
  sudo apt install fuse3      # Debian, Ubuntu
  ```

  ```bash
  sudo dnf install fuse3      # Fedora
  ```

No Corsia command needs root.

## Install

```bash
git clone https://github.com/codewic/corsia
```

```bash
cd corsia && cargo build --release
```

The program is now at `target/release/corsia`. To run it as plain `corsia` from anywhere:

```bash
install -Dm755 target/release/corsia ~/.local/bin/corsia
```

(Make sure `~/.local/bin` is on your `PATH`.)

The live terminal view is a separate build, and optional:

```bash
cargo build --release --manifest-path watch/Cargo.toml
```

## Step 1: see your drives

```bash
corsia lanes
```

This only reads. It lists each drive, the controller it hangs off, and how much bandwidth that controller shares. Read its advice before anything else: a USB 3 disk plugged into a USB 2.0 port is the most common reason a second drive adds nothing.

## Step 2: measure them

```bash
corsia bench
```

**This writes to your drives.** It puts one test file of random bytes (1024 MiB by default) on each drive, reads it back with the page cache bypassed, and deletes it. Choose where the files go, and how big they are, by naming folders:

```bash
corsia bench ~/ /media/me/usbdisk --size 512 --seconds 3
```

Do not point it at a drive you cannot spare the space on, or at installer or recovery media. The result shows each drive alone, all of them at once, and how Corsia would split a read between them.

## Step 3: serve a model from more than one drive

You need a model file in GGUF format (see [choosing the right model file](choosing-a-model-file.md)) and a second drive with room for a copy.

```bash
mkdir -p ~/corsia
```

```bash
corsia serve ~/models/model.gguf --mount ~/corsia --copy-to /media/me/usbdisk
```

What this does:

- The model appears read-only at `~/corsia/model.gguf`. Open that path with your engine, for example `llama-server -m ~/corsia/model.gguf`.
- The original file is not touched or moved.
- A speed copy is written to the second drive in the background, busiest parts first, and reused next time. It takes up to the model's size there; `--copy-gib N` caps it.
- Each read comes from whichever drive will finish it first.
- Live status is at `http://127.0.0.1:7714/api/status`.
- Ctrl+C unmounts.

`corsia serve` also prints one optional command that needs root (raising the mount's read-ahead). It is worth running: in our tests it made reads through the mount 10 to 25% faster.

With only one drive, and that drive a hard disk, a copy laid out expert by expert on the same disk can still help a mixture-of-experts model. The [README](../README.md) shows that form.

## What to expect

Be realistic about the first run:

- **Through the mount, Corsia is slower than reading the file directly** (62 to 81% of direct in our tests), because of how the kernel passes reads to a mount. It pays off when the model is bigger than your RAM and a second drive adds real bandwidth.
- **Through the engine socket it matches direct reads**, and a batch is spread over every drive at once. That needs an engine that speaks the socket: the patches for llama.cpp are in [integrations/llama.cpp](../integrations/llama.cpp/README.md).
- The biggest measured gains so far are for mixture-of-experts models that do not fit in memory, with the llama.cpp patches. The numbers and the machines they came from are in [task.md](../task.md).

## If something goes wrong

| What you see | What to do |
| --- | --- |
| `fusermount3` not found | Install `fuse3` (above). |
| The mount folder is "busy" or shows a stale file after a crash | `fusermount3 -u ~/corsia`, then start again. |
| io_uring errors at start | Some containers and hardened systems switch it off. Check `sysctl kernel.io_uring_disabled`: it must be 0. |
| A second drive makes things slower | Run `corsia lanes` again. Two drives on one controller share its bandwidth, and a slow port caps a fast drive. |
| Numbers that look wrong | Run the same command twice before trusting either result, then please [report it](../CONTRIBUTING.md) with the output of `corsia lanes`. |

Every command takes `--json`, and `corsia --help` lists them all.
