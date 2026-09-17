# Ignoring Guest Flushes (`ignore_flush`)

`ignore_flush=on` makes a virtio-block device treat durability barriers as
no-ops:

- `VIRTIO_BLK_T_FLUSH` completes successfully without reaching the backend.
- A writethrough write skips the `fsync` that would normally follow it.

This is the Cloud Hypervisor equivalent of QEMU's `cache=unsafe`, which is
`cache.writeback=on,cache.direct=off,cache.no-flush=on`. Cloud Hypervisor
already defaults to writeback through the host page cache with `direct=off`,
so `ignore_flush=on` supplies the missing `cache.no-flush` part.

## Build version and snapshot compatibility

`CH_BUILD_VERSION` overrides the version derived from Git or Cargo. The Avrea
package sets it to `v53.0`, retaining the deployed upstream package's identity
for Smithy's exact memory-snapshot version check. `CH_EXTRA_VERSION` still
appends a suffix and is unset by that package build. The RPM release and pinned
source revision identify the implementation separately.

This identity is a compatibility assertion, not just a display preference.
Old-to-new and new-to-old KVM disk/CPU snapshot canaries passed in staging,
including a snapshot produced with flushes ignored. Incompatible future
snapshot changes must use a new identity and an explicit migration plan.
Without `CH_BUILD_VERSION`, upstream version selection remains unchanged.

## Usage

```sh
cloud-hypervisor \
    --disk path=/path/to/disk.raw,image_type=raw,ignore_flush=on \
    ...
```

The vhost-user-block backend takes the same option, because the flag has to
be applied where the I/O is issued:

```sh
vhost-user-block --block-backend path=/path/to/disk.raw,socket=/tmp/vub.sock,ignore_flush=on
```

Setting `ignore_flush=on` on a `--disk` that also sets `vhost_user=on` is
rejected at configuration validation time: the VMM does not perform the I/O
for those disks, so honouring the flag there is not possible.

## Durability

Writes still execute, but successful guest flushes no longer acknowledge
durable storage. Host failure may leave reordered or missing writes that
a filesystem journal cannot recover. Guest data still in guest memory can
also be lost, and terminating the VMM can lose in-process QCOW2 metadata.
Treat this as disposable storage; guest fsync cannot restore durability
while suppression is enabled.

Closing a file does not guarantee a successful fsync. Pause drains active
block requests and synchronously flushes backend data and format metadata,
even with guest flush suppression enabled. A flush failure fails pause and
triggers device/vCPU rollback, so callers must not archive or stop the VM
as though pause succeeded. This covers in-process virtio-block backends;
vhost-user backend lifecycle and unplanned termination need separate handling.

If rollback itself fails, `vm.info` reports `PauseFailed`. Retry `vm.resume`
to complete recovery; snapshot and migration requests must not treat this
state as paused or running. CPUs are resumed only after the clock,
hypervisor and devices have recovered.

Pause does not clear the QCOW2 v3 dirty bit. Graceful shutdown attempts to
clear it; an image copied while the VMM is alive, or after forced termination,
can require a refcount rebuild when reopened for writing. This adds restore
work even when the checked pause flush succeeded. Do not clear the marker
while the VM can resume writing.

Pause blocks the VMM thread while draining requests (up to 30 seconds per
disk) and performing synchronous backend flushes. The flush itself has no
deadline, so pause and live-migration downtime can exceed a caller's timeout.
Timing out the client does not cancel the flush. Callers must check the final
VM state before retrying recovery or archiving; the API may remain blocked
until storage I/O finishes.

## When to use it

`ignore_flush=on` fits disposable disks whose contents are not expected to
survive the host: CI and build VMs, test runners, and short-lived sandboxes
whose durable results are written to external storage. Flush-heavy guest
workloads (package installs, container image builds, compile trees) gain the
most.

Do not use it for any disk whose contents must survive a host failure.

## Test environments

The io_uring pause tests check runtime support and print `SKIPPED` when the
kernel or sandbox blocks it. Run with `--nocapture` to see that diagnostic;
the Rust test harness counts an early return as a passing test, not as ignored.
Set `CH_REQUIRE_IO_URING_TESTS=1` to fail instead of skipping. Release validation
must use that setting on an io_uring-capable Linux host.
