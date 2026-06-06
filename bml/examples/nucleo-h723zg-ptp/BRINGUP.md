# NUCLEO-H723ZG PTP Bring-Up Plan

This example brings up Ethernet PTP on the NUCLEO-H723ZG without STM32CubeMX.
The first goal is deliberately small: transmit a raw Ethernet heartbeat frame
from BML and include the STM32H7 Ethernet PTP clock value in the payload.

## Constraints

- Do not depend on STM32CubeMX-generated files.
- Use `bml-svd` for peripheral declarations.
- Include the generated split SVD output in this example so it is self-contained.
- Keep board/core init (cache setup) in BML via a hand-written SCB peripheral
  block. No C shim, CMSIS, or external compiler is required to build.
- Keep ETH MAC, DMA, PHY, and PTP logic in BML.

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

### 1. Example Skeleton

Files:

- `stm32h723zg.target`
- `main.bml`
- `eth_dma.bml`
- `phy_lan8742.bml`
- `ptp_clock.bml`
- `scb.bml`
- `svd/*.bml`
- `build.sh`
- `flash.sh`

### 2. Board/Core Init

Provide a BML function `board_init()` in `main.bml`, backed by a hand-written
SCB peripheral block in `scb.bml`:

- Keep D-cache disabled (`SCB.CCR.DC = false`).
- Optionally enable I-cache (`SCB.ICIALLU = 0`, then `SCB.CCR.IC = true`),
  ordered with `dsb`/`isb`.
- Leave clock, GPIO, ETH, DMA, PHY, and PTP logic to BML unless a hard hardware
  blocker appears.

### 3. PHY Link

In BML:

- Enable GPIO and ETH clocks.
- Configure RMII pins.
- Reset ETH MAC/DMA.
- Probe LAN8742 PHY over MDIO addresses `0..31`.
- Poll link status.

Success criteria:

- Firmware can tell PHY missing from PHY present but link down.

### 4. TX Heartbeat

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

### 5. PTP Clock

In BML:

- Enable the ETH timestamp unit.
- Initialize seconds/nanoseconds to zero.
- Read the current timestamp seconds/nanoseconds registers.
- Include them in the heartbeat payload.

Success criteria:

- Timestamp increases monotonically.
- Nanoseconds roll over normally.

### 6. RX And Interop Later

After TX works:

- Add RX descriptor and buffer.
- Receive raw frames from the Mac.
- Add RX timestamp extraction.
- Add IPv4/UDP/ARP.
- Interoperate with `ptpd` or another PTP peer.

## Out Of Scope For First Bring-Up

- `ptpd` interop.
- IPv4/UDP.
- ARP.
- PTP servo/clock discipline.
- Sensor sampling alignment.
- ETH PPS output routing.

## Regenerating SVD Files

From this directory:

```sh
rm -rf svd
mkdir -p svd
(cd svd && bml-svd /path/to/STM32H7x3.svd --split)
```

If an H723-specific SVD is available, prefer it over `STM32H7x3.svd`.
