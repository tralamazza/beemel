# NUCLEO-H723ZG PTP Bring-Up Plan

This example brings up Ethernet PTP on the NUCLEO-H723ZG without STM32CubeMX.
The product target is an N:1 system: N STM32H7 mic boards stream timestamped
audio blocks to one STM32H7 switch/controller board. The switch/controller board
will later connect the mic boards through an Ethernet switch; for now, two
NUCLEO-H723ZG boards are the bring-up stand-ins.

The first verified stage is deliberately small: transmit and receive raw
Ethernet frames from BML, include the STM32H7 Ethernet PTP clock value in the
payload, and expose PHY/RX/TX/PTP state in RAM for debugger inspection.

Do not add IPv4, UDP, or ARP unless a concrete tooling or deployment requirement
appears. The core product network is a closed Layer 2 Ethernet network.

## Constraints

- Do not depend on STM32CubeMX-generated files.
- Use `bml-svd` for peripheral declarations.
- Include the generated split SVD output in this example so it is self-contained.
- Keep board/core init (cache setup) in BML via generated `svd/scb.bml` plus
  the hand-written `cache.bml` supplement. No C shim, CMSIS, or external
  compiler is required to build.
- Keep ETH MAC, DMA, PHY, and PTP logic in BML.
- Keep core sync, health, and audio transport on raw Layer 2 Ethernet.
- Treat laptop access as debug/factory tooling through raw Ethernet frames, not
  as a reason to add an IP stack early.

## Product Network Model

- Switch/controller board:
  - PTP grandmaster.
  - Health monitor.
  - Audio collector.
  - Later connects to the mic boards through a switch.
- Mic boards:
  - PTP slaves.
  - Audio producers.
  - Health responders.
  - Never need to talk to each other.
- PTP transport: IEEE 1588v2 over Layer 2 Ethernet, ethertype `0x88f7`.
- Product transport: custom raw Ethernet protocol, currently using ethertype
  `0x88b5` for bring-up frames.

Use the NUCLEO boards like this before switch-board hardware is available:

- Board A: switch/controller stand-in.
- Board B: mic-board stand-in.
- Optional laptop: sniffer/debug tool, connected directly or through an unmanaged
  switch.

## Source Of Truth

- Board: NUCLEO-H723ZG, STM32H723ZG, MB1364.
- CMSIS headers: [STMicroelectronics/cmsis-device-h7](https://github.com/STMicroelectronics/cmsis-device-h7), especially
  `Include/stm32h723xx.h`.
- SVD: generated with `bml-svd` from the closest ST H7 SVD available in the
  CMSIS-SVD data mirror. If ST publishes an H723-specific SVD locally, regenerate
  from that file instead.

The ST `cmsis-device-h7` repository does not currently contain SVD files; it
contains CMSIS headers, startup files, and linker templates.

## Memory Strategy

Use D2 AHB SRAM at `0x30000000` as the BML RAM region for first bring-up.
ETH DMA can access this memory. DTCM at `0x20000000` is not suitable for ETH DMA.

For the first TX-only test, keep D-cache disabled in `board_init` (BML). This
avoids DMA coherency failures where the CPU and ETH DMA see different descriptor
or packet buffer contents. After TX/RX is stable, replace this with an MPU
non-cacheable region for ETH descriptors and buffers.

## Milestones

### 1. Example Skeleton - Done

Files:

- `stm32h723zg.target`
- `main.bml`
- `eth_dma.bml`
- `phy_lan8742.bml`
- `ptp_clock.bml`
- `cache.bml`
- `svd/*.bml`
- `build.sh`
- `flash.sh`

### 2. Board/Core Init - Done

Provide a BML function `board_init()` in `main.bml`, backed by generated
`svd/scb.bml` plus the hand-written `cache.bml` I-cache invalidate register:

- Keep D-cache disabled (`SCB.CCR.DC = false`).
- Optionally enable I-cache (`SCB.ICIALLU = 0`, then `SCB.CCR.IC = true`),
  ordered with `dsb`/`isb`.
- Leave clock, GPIO, ETH, DMA, PHY, and PTP logic to BML unless a hard hardware
  blocker appears.

### 3. PHY Link - Done

In BML:

- Enable GPIO and ETH clocks.
- Configure RMII pins.
- Reset ETH MAC/DMA.
- Probe LAN8742 PHY over MDIO addresses `0..31`.
- Poll link status.

Success criteria:

- Firmware can tell PHY missing from PHY present but link down.
- Firmware detects LAN8742 at PHY address `0` on tested NUCLEO hardware.
- Firmware polls link after autonegotiation and reports link up.

### 4. TX Heartbeat - Done

In BML:

- Allocate one TX descriptor and one TX packet buffer in D2 SRAM.
- Fill a broadcast Ethernet frame:

```text
dst       = ff:ff:ff:ff:ff:ff
src       = 02:00:00:00:72:3a
ethertype = 0x88b5
payload   = "BMLPTP" + seq + ptp_seconds + ptp_nanoseconds
```

- Send one frame per second.

Mac verification:

```sh
sudo tcpdump -ni <iface> -e -xx 'ether proto 0x88b5'
```

Success criteria:

- The Mac sees one frame per second.
- Sequence number increments.
- Source MAC matches `02:00:00:00:72:3a`.

Verified with:

```sh
sudo tcpdump -U -ni en8 -e -xx -c 5 'ether proto 0x88b5'
```

Observed one frame per about one second, sequence incrementing, and monotonic PTP
fields in the payload.

### 5. PTP Clock - Done For Free-Running Clock

In BML:

- Enable the ETH timestamp unit.
- Initialize seconds/nanoseconds to zero.
- Read the current timestamp seconds/nanoseconds registers.
- Include them in the heartbeat payload.

Success criteria:

- Timestamp increases monotonically.
- Nanoseconds roll over normally.

This is not yet a disciplined PTP clock. It is the local ETH timestamp unit
running from the current reset clock tree/addend assumptions.

### 6. RX Raw Frames - Done

In BML:

- Add RX descriptor and buffer.
- Receive raw frames from the Mac.
- Add RX timestamp extraction.

Success criteria:

- Firmware receives raw Ethernet frames from a host.
- Firmware exposes last RX length, ethertype, source MAC, packet count, and RX
  timestamp context data in debugger-visible RAM.

Verified by sending IPv6 multicast from macOS on `en8`; firmware received
ethertype `0x86dd` frames from source MAC `6c:1f:f7:be:86:43` and captured RX
timestamp context descriptors.

### 7. Split Two-Board Roles - Next

Refactor the example into explicit roles:

- `main_controller.bml`: switch/controller stand-in.
- `main_mic_node.bml`: mic-board stand-in.
- Shared modules stay in `eth_dma.bml`, `phy_lan8742.bml`, `ptp_clock.bml`, and
  new protocol modules.

Success criteria:

- Each role builds from this example directory.
- Both roles still use the same target, generated SVD, PHY, ETH DMA, and PTP
  helpers.
- The controller role can be flashed to one NUCLEO and the mic-node role to the
  other.

### 8. Product Layer 2 Health Protocol

Define a small custom product protocol on ethertype `0x88b5`.

Common header fields:

```text
magic/version
message_type
board_id
sequence
ptp_seconds
ptp_nanoseconds
payload_len
payload
```

Initial message types:

- `BOOT_HELLO`
- `HEALTH_PING`
- `HEALTH_STATUS`
- `SYNC_STATUS`
- `AUDIO_TEST_BLOCK`

Success criteria:

- Controller sends `HEALTH_PING`.
- Mic node replies `HEALTH_STATUS`.
- Status includes board ID, firmware/build marker, PHY state, PTP state, RX/TX
  counters, timestamp counters, and audio-test counters once they exist.

### 9. Layer 2 PTP Skeleton

Use ethertype `0x88f7`. Do not add IPv4/UDP/ARP for this milestone.

First target: collect the four PTP timestamps, not discipline the clock yet.

- Controller sends `Sync`.
- Controller sends `Follow_Up` if using two-step mode.
- Mic node receives `Sync`/`Follow_Up` and records master transmit time.
- Mic node sends `Delay_Req`.
- Controller receives `Delay_Req` and replies `Delay_Resp`.

Required support:

- TX timestamp extraction for PTP event frames.
- RX timestamp extraction already exists.
- Basic PTPv2 Layer 2 message parsing/building.

Success criteria:

- Mic node records `t1`, `t2`, `t3`, `t4` for repeated exchanges.
- Offset/path-delay estimates are exposed in debugger-visible RAM.

### 10. Minimal PTP Slave Servo

After timestamp exchange works:

- Estimate offset and path delay on the mic node.
- First expose offset/drift only.
- Then adjust the local ETH PTP clock offset/addend.
- Keep a lock/state estimate for health reporting.

Success criteria:

- Mic node converges toward controller PTP time.
- Health/status frames report current offset estimate and lock state.

### 11. Audio Test Blocks

Before real microphones, simulate audio blocks from the mic node.

Each audio-test frame should include:

- board ID
- sequence number
- first-sample PTP timestamp
- sample rate
- channel count
- sample count
- synthetic payload

Success criteria:

- Controller receives continuous block sequence.
- Controller can detect drops/reorders.
- First-sample timestamps advance by the expected sample count/sample-rate
  interval.

### 12. Real Audio Capture Later

After network timing is stable:

- Add SAI/I2S/PDM capture depending on the microphone hardware.
- Timestamp the first sample of each DMA block with the disciplined PTP clock.
- Stream real audio blocks using the same frame format as `AUDIO_TEST_BLOCK`.

## Out Of Scope For First Bring-Up

- IPv4/UDP.
- ARP.
- Laptop-native `ping`, HTTP, SSH, or UDP tooling.
- Switch configuration.
- Switch transparent-clock/boundary-clock behavior.
- `ptpd`/`linuxptp` interop.
- PTP servo/clock discipline.
- Real microphone sampling alignment.
- ETH PPS output routing.

## Laptop Access

Linux/macOS laptops can still access the boards during debug/factory workflows by
sending and receiving raw Ethernet frames.

- Receive/sniff with `tcpdump` or Wireshark.
- Send custom frames with a small host tool using raw sockets or `libpcap`.
- Root/admin privileges are usually required.
- No IP address is required on the board.

Example sniff command:

```sh
sudo tcpdump -U -ni <iface> -e -xx 'ether proto 0x88b5 or ether proto 0x88f7'
```

Add IPv4/UDP/ARP only if raw Ethernet tooling becomes an actual product or
factory-test blocker.

## Regenerating SVD Files

From this directory:

```sh
rm -rf svd
mkdir -p svd
(cd svd && bml-svd /path/to/STM32H7x3.svd --split)
```

If an H723-specific SVD is available, prefer it over `STM32H7x3.svd`.
