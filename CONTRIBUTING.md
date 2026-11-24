# Contributing

Corsia is early. Bug reports with numbers are the most useful thing you can send.

## Reporting

Say what machine it was (CPU, RAM, each drive and how it is connected: `corsia lanes` prints this), the model file, the exact command, and what you measured. A result that is slower than expected is a bug worth reporting.

## Building

```bash
cargo build --release
```

Linux only for now. It needs `fusermount3` for the mount and a kernel with io_uring. `cargo test` runs the unit tests; they need no drives or models.

## Changes

- Keep claims measured. A speed claim comes with the command that produced it and the hardware it ran on.
- Never benchmark removable media that holds something the user needs. `corsia bench` writes test files; say so in anything that calls it.
- One change per pull request, with a plain description of what it does and how you checked it.
- The llama.cpp patches in `integrations/llama.cpp` apply in order to the tag named in that folder's README. If you change one, re-export the series.

## Licence

Contributions are accepted under the same terms as the project: MIT or Apache-2.0, at the user's choice.
