# Runtime assurance

HyperOS's isolation promises are written as formal requirements and checked
while the host runs, using three NASA open-source tools:

| Step | Tool | Licence | Output |
| --- | --- | --- | --- |
| Write requirements in structured English | [FRET](https://github.com/NASA-SW-VnV/fret) | Apache-2.0 | `requirements/host.json` (hand-written) |
| Formalise them into past-time temporal logic | FRET's parser (`tools/formalize.js`) | Apache-2.0 | `requirements/host.fcs.json` (generated) |
| Generate a monitor program | [Ogma](https://github.com/nasa/ogma) | Apache-2.0 | `Copilot.hs` |
| Compile it to constant-memory C | [Copilot](https://github.com/Copilot-Language/copilot) | BSD-3 | `vmm-daemon/src/assurance/generated/monitor.{c,h}` |

`vmm-daemon` links the C monitor through `vmm-daemon/src/assurance/shim.c` and
samples it after every request and every 2 seconds. See "Runtime assurance" in
[`ARCHITECTURE.md`](../ARCHITECTURE.md) for the requirements and what the daemon
does on a violation.

## Changing a requirement

1. Edit `requirements/host.json` (FRETish text, variables).
2. Regenerate: `FRET_DIR=/path/to/fret assurance/tools/generate.sh`
   (needs FRET's npm dependencies, `ogma`, and GHC with the copilot libraries;
   the versions are pinned in `.github/workflows/assurance.yml`). If you don't
   have the Haskell toolchain, open a PR: the **Assurance monitors** workflow
   regenerates the files and uploads them as the `assurance-monitor` artifact.
3. If you added or renamed a variable or requirement, update `shim.c` and
   `REQUIREMENTS` in `vmm-daemon/src/assurance.rs` (a unit test checks the
   table against `host.json`).
4. Commit the regenerated files. CI fails if they don't match the requirements.

## Not done yet

- FRET realizability and consistency checking (needs the Kind 2 or JKind solver).
- "No crosvm process runs as UID 0" (waits for A2).
