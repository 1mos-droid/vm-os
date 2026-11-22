# Cloud & Nested-Virtualization Testing Guide (`docs/CLOUD_TESTING.md`)

This guide provides exact, reproducible commands to boot the compiled hybrid UEFI/BIOS HyperOS disk image (`hyperos.img`) on a nested-virtualization cloud VM with a strict **4 GiB RAM cap (`-m 4G`)** and verify:

1. Host idle RAM acceptance target (`< 350 MiB` after 60 seconds idle)
2. `runit` service supervision, `/dev/kvm`, and Btrfs CoW storage (`/var/lib/hyperos`)
3. **ZRAM** (`zstd`, 65% of `MemTotal`, swap priority `100`)
4. **KSM** setting (`/sys/kernel/mm/ksm/run`; see the note in §4.4)
5. **`crosvm` & `vmm-daemon`** launch behavior over `/run/hyperos/vmm.sock`

All paths, limits, and JSON-RPC 2.0 payloads match [`ARCHITECTURE.md`](../ARCHITECTURE.md) verbatim.

---

## 1. Provision a Nested-Virtualization Cloud VM

Choose an `x86_64` cloud instance that exposes hardware virtualization (`vmx` or `svm`) to nested guests:

- **Google Cloud:** N2, C2, or N2D instance created with `--enable-nested-virtualization` (minimum 4 vCPUs, 8 GiB host RAM so the nested 4 GiB QEMU guest runs cleanly).
- **AWS:** Bare-metal instance (`c6i.metal`, `m6i.metal`, or `c7a.metal-16xl`).
- **Azure:** Dv3/Ev3 or Dv5/Ev5 series VM with nested virtualization enabled.

On the cloud VM (Debian/Ubuntu host), install QEMU and disk utilities, then verify KVM and nested virtualization are active:

```sh
sudo apt-get update
sudo apt-get install -y qemu-system-x86 btrfs-progs parted e2fsprogs socat jq

# Verify CPU virtualization extensions and /dev/kvm
egrep -c '(vmx|svm)' /proc/cpuinfo
ls -la /dev/kvm

# Verify nested KVM parameter on the outer host (expected: Y or 1)
cat /sys/module/kvm_intel/parameters/nested 2>/dev/null || \
cat /sys/module/kvm_amd/parameters/nested 2>/dev/null
```

If `nested` prints `N` or `0` on Intel/AMD bare metal, reload the KVM module with `nested=1` before proceeding:

```sh
# Intel:
sudo modprobe -r kvm_intel && sudo modprobe kvm_intel nested=1
# AMD:
# sudo modprobe -r kvm_amd && sudo modprobe kvm_amd nested=1
```

---

## 2. Obtain and Prepare `hyperos.img`

### Option A: Download from GitHub Actions CI Artifact

```sh
# Download the latest hyperos-x86_64-hybrid-<sha> artifact from the Build HyperOS workflow
# (the full image build runs on pushes to main and manual dispatch, not on PRs)
gh run download --pattern 'hyperos-x86_64-hybrid-*' --dir ./hyperos-artifact
cp ./hyperos-artifact/hyperos-x86_64-hybrid-*/hyperos.img ./hyperos.img
```

### Option B: Build Locally (Pinned Baseline)

Follow the exact build sequence from [`ARCHITECTURE.md`](../ARCHITECTURE.md) (Buildroot `2025.02.1`, Linux `6.12.20`, runit `2.1.2`):

```sh
scripts/validate-configs.sh
export HOSTCC="$PWD/scripts/host-gcc-gnu17"
export BR2_CMAKE_CANDIDATES=""
make -C buildroot O="$PWD/output" BR2_EXTERNAL="$PWD/kernel-buildroot" hyperos_x86_64_defconfig
scripts/validate-configs.sh --resolved-buildroot output/.config
make -C buildroot O="$PWD/output" -j2
scripts/validate-configs.sh --resolved-kernel output/build/linux-6.12.20/.config
scripts/validate-configs.sh --image output/images/hyperos.img
cp output/images/hyperos.img ./hyperos.img
```

### Give the Data Partition Room to Grow

`hyperos.img` has three GPT partitions (see `ARCHITECTURE.md`): `HYPEROS_EFI`
(FAT32, kernel + initramfs), a 512 MiB read-only EROFS root, and a 512 MiB Btrfs
`HYPEROS_DATA` partition. In **Persistent Mode** the initramfs grows partition 3
and its Btrfs filesystem into trailing free space on first boot, so you only need
to enlarge the image file:

```sh
truncate -s 8G hyperos.img
```

### Import a Guest Base Image and Run the Release Check

The default configuration builds pinned crosvm and minijail sources, including
x86_64 sandbox policies. Host build prerequisites now also include libclang
and a Rust musl target; see [`kernel-buildroot/README.md`](../kernel-buildroot/README.md).
Import only into an **offline** image, after building a guest:

```sh
sudo guest-images/build-alpine-guest.sh --out-dir ./out
sudo scripts/import-guest-image.py ./out/alpine-3-21-virtio hyperos.img
```

The importer validates the manifest and both source and copied checksums,
preserves sparse extents, publishes atomically and refuses to replace an image
ID. It unmounts and detaches its loop device on success, failure and catchable
termination. Never run it against a disk attached to a running VM.

For the full build/import/boot flow on nested KVM:

```sh
sudo scripts/test-v1-release.sh --image output/images/hyperos.img
# Reuse a previously built guest:
sudo scripts/test-v1-release.sh --image output/images/hyperos.img --guest-dir out/alpine-3-21-virtio
```

This uses a scratch image, starts an isolated VM through the daemon, and requires
Alpine's userspace marker over vsock, KSM off, and headless idle RAM below
350 MiB. `--dry-run` prints the plan without root or KVM. Serial logs are retained.
The runner does not automate persistence across a host reboot, graphical RAM
measurement, Secure Boot or update rollback tests. These need separate hardware
acceptance; the implementation design is in
[`VERIFIED_BOOT.md`](../kernel-buildroot/VERIFIED_BOOT.md).

---

## 3. Boot HyperOS with a 4 GB RAM Cap (`-m 4G`)

Launch HyperOS inside nested KVM with an exact **4 GB RAM cap (`-m 4G`)** and serial console on `ttyS0`:

```sh
qemu-system-x86_64 \
    -enable-kvm \
    -cpu host \
    -smp 4 \
    -m 4G \
    -nographic \
    -drive file=hyperos.img,format=raw,if=virtio
```

*(Press `Ctrl-A` then `X` to exit QEMU's serial console if needed. For clean guest shutdown, use `runit-init 0` inside HyperOS.)*

Because the base HyperOS image intentionally starts no login shell by default (`ARCHITECTURE.md`), for interactive verification on a cloud test VM you can interrupt GRUB (`e` during the 3-second timeout) and append `init=/bin/sh` only for low-level debugging, or run a temporary serial getty service in a test overlay while verifying live `runit` supervision.

---

## 4. Verify Host Acceptance Targets (RAM, `runit`, `/dev/kvm`, Btrfs, ZRAM, KSM)

Run the following verification commands inside the booted 4 GB HyperOS VM.

### 4.1 `runit` Supervision, `/dev/kvm`, and Btrfs Storage

```sh
# Check runit readiness markers created by /etc/sv/{zram,ksm,storage}/run
ls -la /run/hyperos/ready/
sv status /run/service/*

# Verify nested KVM is available inside HyperOS
ls -la /dev/kvm

# Verify root is mounted read-only and /var/lib/hyperos is Btrfs with zstd
awk '$2 == "/" || $2 == "/var/lib/hyperos" { print }' /proc/mounts
stat -c '%a %U:%G %n' /var/lib/hyperos /var/lib/hyperos/base-images /var/lib/hyperos/instances
```

Expected results:
- `/run/hyperos/ready/zram`, `/run/hyperos/ready/ksm`, and `/run/hyperos/ready/storage` all exist.
- `/dev/kvm` exists (`crw-------` or `crw-rw----`).
- `/` is an `overlay` mount (read-only EROFS lower layer, 128 MiB tmpfs upper layer); `/var/lib/hyperos` is `btrfs` mounted `rw,noatime,compress=zstd`, mode `700`, owned by `root:root`.

### 4.2 Idle Host RAM Target (`< 350 MiB` after 60 s Idle)

Wait 60 seconds after boot with **no VM running**, then record `/proc/meminfo`:

```sh
sleep 60
awk '
  /^MemTotal:/     { total = $2 }
  /^MemAvailable:/ { avail = $2 }
  END {
    used = total - avail
    printf "MemTotal: %d KiB (%.1f MiB)\nMemAvailable: %d KiB (%.1f MiB)\nUsed: %d KiB (%.1f MiB)\n",
           total, total/1024, avail, avail/1024, used, used/1024
    if (used < 358400) print "PASS: used RAM < 358400 KiB (350 MiB)";
    else { print "FAIL: used RAM >= 358400 KiB"; exit 1 }
  }
' /proc/meminfo
```

### 4.3 Verify ZRAM (`zstd`, 65% of `MemTotal`, Priority 100)

```sh
# 1. Confirm /dev/zram0 is active swap at priority 100
cat /proc/swaps

# 2. Confirm active compression algorithm is [zstd]
cat /sys/block/zram0/comp_algorithm

# 3. Confirm disksize matches 65% of MemTotal (~2.6 GiB on a -m 4G VM)
cat /sys/block/zram0/disksize
awk '/^MemTotal:/ { printf "Expected 65%% disksize: %d bytes\n", $2 * 1024 * 65 / 100 }' /proc/meminfo

# 4. Inspect live ZRAM statistics (orig_data_size compr_data_size mem_used_total ...)
cat /sys/block/zram0/mm_stat
```

Expected results:
- `/proc/swaps` lists `/dev/zram0` with `Priority` = `100`.
- `/sys/block/zram0/comp_algorithm` highlights `[zstd]`.
- `/sys/block/zram0/disksize` equals `MemTotal_KiB * 1024 * 65 / 100` (approximately `2730000000` bytes on a `-m 4G` VM).
- `/sys/block/zram0/mm_stat` prints whitespace-separated integer counters readable by `vm-control-ui`.

### 4.4 Inspect KSM

```sh
# 1. KSM must stay disabled by default (expected: 0)
cat /sys/kernel/mm/ksm/run

# 2. Inspect KSM merge counters before and after starting two identical guests
grep . /sys/kernel/mm/ksm/pages_shared \
       /sys/kernel/mm/ksm/pages_sharing \
       /sys/kernel/mm/ksm/pages_unshared \
       /sys/kernel/mm/ksm/full_scans
```

Neither `vmm-daemon` nor `crosvm` currently marks guest memory `MADV_MERGEABLE`, so expect `pages_sharing` to stay at 0 even with two identical guests running.

> **Security note (`ARCHITECTURE.md`):** Cross-VM page sharing is a side channel and is unsuitable for untrusted guests.

---

## 5. Verify `crosvm` Launch & `vmm-daemon` JSON-RPC Behavior

### 5.1 Direct `crosvm` Nested-KVM Smoke Test

Verify that `/usr/bin/crosvm` launches cleanly on nested `/dev/kvm` with jail/seccomp, `virtio-blk`, `virtio-balloon`, and `virtio-vsock`:

```sh
# Clone a scratch CoW disk via strict Btrfs reflink (zero initial disk overhead)
btrfs subvolume create /var/lib/hyperos/instances/smoke-test
cp --reflink=always \
    /var/lib/hyperos/base-images/alpine-3-21-virtio/disk.raw \
    /var/lib/hyperos/instances/smoke-test/disk.raw

# Boot guest with crosvm (512 MiB ceiling, 1 vCPU, balloon enabled, control socket)
/usr/bin/crosvm run \
    --cpus 1 \
    --mem 512 \
    --block path=/var/lib/hyperos/instances/smoke-test/disk.raw,root \
    --vsock cid=3 \
    --socket /run/hyperos/smoke-crosvm.sock \
    --serial type=stdout,hardware=serial,console,stdin \
    --params "root=PARTUUID=48504131-01 rw console=ttyS0" \
    /boot/bzImage
```

In a second shell (or backgrounded test), verify dynamic `virtio-balloon` deflation/inflation (e.g., shrinking usable guest RAM from 512 MiB to 256 MiB => `balloon_bytes = (512 - 256) * 1048576 = 268435456`):

```sh
/usr/bin/crosvm balloon 268435456 /run/hyperos/smoke-crosvm.sock
/usr/bin/crosvm balloon_stats /run/hyperos/smoke-crosvm.sock
/usr/bin/crosvm stop /run/hyperos/smoke-crosvm.sock
```

Clean up the scratch subvolume after the direct test:

```sh
btrfs subvolume delete /var/lib/hyperos/instances/smoke-test
```

### 5.2 Verify `vmm-daemon` Unix Socket & JSON-RPC 2.0 Contract

Verify socket permissions (`root:root`, mode `0600`, parent `/run/hyperos` mode `0700`):

```sh
stat -c '%a %U:%G %n' /run/hyperos /run/hyperos/vmm.sock
```

Exercise every lifecycle method over `/run/hyperos/vmm.sock` using `socat` (exact LF-delimited frames from `ARCHITECTURE.md`):

```sh
# 1. list_vms
printf '%s\n' '{"jsonrpc":"2.0","id":"list-1","method":"list_vms","params":{}}' \
  | socat - UNIX-CONNECT:/run/hyperos/vmm.sock

# 2. create_vm (returns stopped; allocates UUID and Btrfs reflink disk)
CREATE_RESP="$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":"create-1","method":"create_vm","params":{"name":"cloud-test","base_image_id":"alpine-3-21-virtio","vcpu_count":1,"ram_mib":1024,"network_mode":"isolated","disposable":false}}' \
  | socat - UNIX-CONNECT:/run/hyperos/vmm.sock)"
echo "${CREATE_RESP}"
VM_ID="$(echo "${CREATE_RESP}" | jq -r '.result.vm_id')"

# 3. start_vm
printf '{"jsonrpc":"2.0","id":"start-1","method":"start_vm","params":{"vm_id":"%s"}}\n' "${VM_ID}" \
  | socat - UNIX-CONNECT:/run/hyperos/vmm.sock

# 4. set_balloon_ram (target 512 MiB usable RAM while running)
printf '{"jsonrpc":"2.0","id":"balloon-1","method":"set_balloon_ram","params":{"vm_id":"%s","ram_mib":512}}\n' "${VM_ID}" \
  | socat - UNIX-CONNECT:/run/hyperos/vmm.sock

# 5. pause_vm -> resume via start_vm -> stop_vm (hard power-off)
printf '{"jsonrpc":"2.0","id":"pause-1","method":"pause_vm","params":{"vm_id":"%s"}}\n' "${VM_ID}" \
  | socat - UNIX-CONNECT:/run/hyperos/vmm.sock
printf '{"jsonrpc":"2.0","id":"stop-1","method":"stop_vm","params":{"vm_id":"%s"}}\n' "${VM_ID}" \
  | socat - UNIX-CONNECT:/run/hyperos/vmm.sock

# 6. snapshot_vm (cold Btrfs read-only snapshot; allowed only while stopped)
printf '{"jsonrpc":"2.0","id":"snap-1","method":"snapshot_vm","params":{"vm_id":"%s"}}\n' "${VM_ID}" \
  | socat - UNIX-CONNECT:/run/hyperos/vmm.sock

# 7. set_network_mode (allowed only while stopped; applies on next start)
printf '{"jsonrpc":"2.0","id":"net-1","method":"set_network_mode","params":{"vm_id":"%s","network_mode":"host-only"}}\n' "${VM_ID}" \
  | socat - UNIX-CONNECT:/run/hyperos/vmm.sock
```

### 5.3 Verify 4 GB RAM Admission Control (`-32003`)

On a `-m 4G` VM, `MemTotal - 768 MiB` is roughly `3328 MiB` (and ZRAM swap does **not** count toward admission capacity). Starting a VM whose `ram_mib` pushes active running/paused guest ceilings above `MemTotal - 768 MiB` must be rejected with error code `-32003` (`insufficient RAM/storage/CPU resources`).

---

## 6. Verify Clean Shutdown

Shut down HyperOS cleanly via `runit`:

```sh
runit-init 0
```

Confirm stage 3 (`/etc/runit/3`) stops `vmm-daemon` before running `swapoff /dev/zram0`, `sync`, `umount /var/lib/hyperos`, and `mount -o remount,ro /` (a no-op on the overlay root), and QEMU exits cleanly without Btrfs corruption.
