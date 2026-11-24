# Choosing the right model file for your CPU

The same model is published in many files: different sizes, and different weight formats inside each file. Two things decide how fast one of them runs on your machine:

1. **Whether it fits in memory.** That decides how much has to come from the drives.
2. **Which weight formats it uses.** That decides how fast your CPU does the maths, and it differs a lot from CPU to CPU.

Most guides stop at the first point. This one is about the second, because it is free speed that almost nobody takes.

## Why the format matters

A weight format (Q2_K, Q3_K, Q4_K, Q8_0, F16 and so on) is a way of packing numbers. To use them, the CPU unpacks and multiplies them with special instructions (AVX, AVX2, FMA, F16C, AVX-512). Each format has its own code path for each instruction set, and some paths are much better than others. So a format with fewer bits is not always faster.

Measured on an Intel i5-2450M (2011, AVX only: no AVX2, FMA or F16C), time for one expert multiplication, lower is better:

| Format | Microseconds | Note |
| --- | --- | --- |
| Q2_K | 776 | fastest here |
| Q4_K | 826 | |
| Q4_0 | 912 | |
| IQ4_XS | 1,008 | |
| Q8_0 | 1,067 | |
| Q6_K | 1,101 | |
| **Q3_K** | **1,280** | slower than Q4_K, with fewer bits |
| IQ2_XS | 1,573 | |
| F32 | 3,216 | |
| F16 | 7,360 | no F16C on this CPU, so every number is converted in software |

On this CPU, Q3_K is half as slow again as Q4_K. A newer CPU with AVX2 gives a different table. That is the whole point: **the order depends on your processor, so measure it there.**

## Mixed files hide slow tensors

Many popular files are "dynamic" mixes: the name says Q2 or Q4, but inside, different tensors use different formats. The file we tested (Qwen3-30B-A3B, Unsloth UD-Q2_K_XL) stores 37 of its 48 expert down projections in Q3_K, the slowest common format on this CPU. You cannot see that from the file name.

## What tuning the file bought

`integrations/llama.cpp/cpu-tune.sh` times every format on the CPU it runs on, then writes a copy of the model in which each tensor is moved to the fastest format that has **at least as many bits per weight** as the original. It never makes a tensor less precise. Everything else is copied byte for byte.

On the i5-2450M, with a 10 GiB memory cap:

| | Original | Tuned copy |
| --- | --- | --- |
| Size | 11,267 MiB | 12,211 MiB |
| Speed (two 256-token answers, mean) | 3.71 tokens/s | **4.11 tokens/s (+11%)** |
| Perplexity | 8.3454 | 8.3459 |

Quality against the original: KL divergence 0.0085, same top token 94.8% of the time. The copy took 7.4 minutes to write.

Be clear about what this is: the tuned copy re-rounds weights that were already rounded to 3 bits, so it cannot be more accurate than the original. It is the same model in a shape your CPU computes faster. A file quantised to the faster format straight from the full-precision weights would be better still, if one exists for your model.

## How to pick a file

1. **See what your CPU has.**

   ```bash
   grep -o -w -E 'avx|avx2|fma|f16c|avx512f' /proc/cpuinfo | sort -u
   ```

2. **Pick the size by memory.** If the file fits in free RAM, any drive will do. If it does not, use a mixture-of-experts model and let Corsia stream the experts: only a few are used per token.
3. **Avoid the known slow cases.**
   - No `f16c` in the list: avoid F16 tensors.
   - `avx` but no `avx2`: prefer Q2_K, Q4_K and Q4_0. Q3_K and the IQ formats are slow.
   - With `avx2`, llama.cpp repacks Q4_K weights in memory at load. For a model larger than RAM that reads the whole file first (13 minutes for an 18.6 GB file on a hard disk in our test). The Corsia patches turn repacking off while streaming; set `CORSIA_REPACK=1` to keep it.
4. **Measure, then tune.**

   ```bash
   DRY=1 integrations/llama.cpp/cpu-tune.sh MODEL.gguf
   ```

   shows which tensors would change and why. Without `DRY=1` it writes `MODEL-cpu.gguf`. `MIN_GAIN` (default 1.10) and `MAX_GROWTH` (default 1.35) set how much faster a format must be and how much bigger the tensor may get. It needs the llama.cpp fork built with `-DLLAMA_BUILD_TESTS=ON`.
5. **Check threads.** On the two-core laptop, 4 threads (hyperthreading) were 4 to 8% slower than 2 in every format. Use the number of real cores first, then try more.

## Things that did not help (so you can skip them)

All measured on the same laptop: a hand-rewritten AVX Q3_K routine (no faster), 2 MiB huge pages (slower in the kernel test), spinning instead of sleeping while waiting (noise). Heat matters more than any of them: the CPU throttles above about 90 C, so long runs slow down unless it is kept cool.

## Limits of these numbers

One laptop CPU, one model. The method carries over; the table does not. Run the measurement on your own machine before trusting any ranking, including this one.
