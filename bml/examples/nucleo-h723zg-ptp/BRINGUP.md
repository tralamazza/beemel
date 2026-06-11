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

KNOWN GAP -- no link-bounce recovery (observed live 2026-06-11, cost an
hour of false regression hunting): on a Nucleo the peer board's NRST
also hard-resets its PHY, so resetting board B bounces the link for
board A. If board A is mid-TX when the link drops, the EQOS TX queue
stalls permanently and `tx_wait_idle` spins forever. The wedge
SIGNATURE, for next time: TX_FRAME_COUNT frozen (the next eth_send_*
never queues), DMACSR = TBU + RBU + RI (TX suspended; RX ring exhausted
behind the dead main loop), no HardFault, peer's frames still arriving
(RBU proves the link recovered -- only the MAC/DMA state did not).
WORKAROUND until real link-up recovery lands: reset/flash the
TX-active node (the controller) LAST, so its bring-up runs against a
stable link. The real fix is the backlog item: detect the link
down->up edge in phy_poll_link and re-initialize MACCR + re-arm the
TX/RX rings.

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

### 7. Split Two-Board Roles - Done

Refactored the example into explicit roles:

- `main_controller.bml`: switch/controller stand-in, board id 1.
- `main_mic_node.bml`: mic-board stand-in, board id 2.
- Shared power-on path lifted into `board.bml` (`board_bringup()` runs cache/core
  init plus the ETH/PHY/PTP init sequence; `delay()` for the heartbeat loop).
- Shared modules stay in `eth_dma.bml`, `phy_lan8742.bml`, `ptp_clock.bml`, and
  `board.bml`.

`build.sh` and `flash.sh` take a role argument (`controller` or `mic_node`) and
emit/flash `main_<role>.{o,ld,elf,bin}`. The ETH DMA descriptors and buffers
take their addresses from `&STATIC as u32`, so the linker symbol is the single
source of truth -- no hardcoded addresses and no build-time address-drift guard.

For this milestone the two roles share the heartbeat path and source MAC and
differ only by a board id byte at payload offset 32. The two binaries differ by
exactly those two immediate bytes. Per-board MACs and a real header arrive in
milestone 8.

Success criteria:

- Each role builds from this example directory. Done.
- Both roles still use the same target, generated SVD, PHY, ETH DMA, and PTP
  helpers. Done.
- The controller role can be flashed to one NUCLEO and the mic-node role to the
  other. Build verified; on-hardware flash of both boards pending.

### 8. Product Layer 2 Health Protocol - Done (Two-Board Validated)

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

Status (2026-06-11): the 74-byte fixed frame (`ProductMsg`, magic "BMH1",
all fields `@be`), send/parse/dispatch, and the controller/mic role wiring
are implemented; `SYNC_STATUS`/`AUDIO_TEST_BLOCK` are defined but not yet
produced. Validated single-board under MAC loopback (`MACCR.LM`,
`main_health_loop.bml`): ping -> looped ping -> status reply -> looped
status -> recorded, with PINGS_SENT == STATUS_SENT == STATUS_SEEN in
lockstep and the status payload's RX counter arithmetically exact.

Two-board validation (2026-06-12, two NUCLEOs direct-cabled, no switch,
auto-MDIX; both probed over their own ST-Links by adapter serial): the
controller's STATUS_SEEN equals the mic node's PINGS_ANSWERED exactly
(3 == 3, then 7 == 7 after a 60-second soak), STATUS_LAST_BOARD = 2,
and the echoed sequence tracks the controller's ping counter. Zero
faults on either board. Known pacing note: the mic services one frame
per `delay()` loop, so it misses pings when its 4-deep ring overflows
-- fine for this milestone, must poll faster for PTP.

A fourth silicon finding came out of bringing this up (bisected V1-V3
live on the wire): with DMACIER RIE/NIE set and DMACSR.RI never
acknowledged, the TX-active controller took an imprecise BusFault
within seconds of real RX traffic; IOC alone was clean, and per-frame
W1C acknowledge (now in eth_poll_rx) is clean. The RX-only bench
tolerated latched RI for thousands of frames, so the micro-mechanism
is recorded, not understood.

Two findings from this milestone, both caught by the toolchain:

- `bml verify` flagged a definite V100 in `rx_get8`: computing the buffer
  base as integer arithmetic inside a helper loses the provenance assume.
  Restructured to index from `&RX_BUFFER` directly; the error (and the
  V110/V113 noise with it) disappeared.
- The TX OWN-bit spin (`tx_wait_idle`) compiled to an infinite `b .`:
  raw-pointer loads of agent-mutated memory are plain LLVM loads, and the
  optimizer hoisted the load out of the empty loop -- the DMA is a
  concurrent writer the optimizer cannot see. Found on hardware
  (TX_FRAME_COUNT frozen at 5, PC parked on the self-branch). Now closed
  in the compiler: accesses through raw pointers into agent-shared memory
  lower as volatile, E620 keeps such pointers from escaping the deriving
  function, and the plain-read spin is sound again (falsified on the board
  both ways). A third silicon find came out of validating that fix: the
  TIM2 PSC=47999 write was silently dropped when a scheduling change
  closed the gap after the RCC clock-enable -- first-write-after-enable is
  lost while the enable propagates. Fix: read the enable bit back before
  touching the peripheral (timer.bml); deriving that read-back from the
  target's declared gates is an open model item.

### 9. Layer 2 PTP Skeleton - Done

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

Status (2026-06-12, two boards direct-cabled): DONE. Real PTPv2 frames
on 0x88F7 (fixed 68-byte shape for Sync/Follow_Up/Delay_Req/Delay_Resp,
`PtpL2` @repr(packed)/@be). TX timestamps via TDES2.TTSE on event
frames, collected from the descriptor write-back (TTSS + TDES0/1)
through the volatile agent-pointer reads; RX timestamps via the
existing context descriptors, associated by the rx_ts_count change
following the frame. Controller syncs every 8th tick (two-step) and
answers Delay_Req with the request's RX hardware timestamp. Measured:
every sync round completes the full four-timestamp exchange
(SYNCS_SENT == DRESP_SENT == mic PTP_ROUNDS across soaks, e.g. 10==10;
T1_MISS == T3_MISS == 0 -- hardware TX timestamping hit on every event
frame). Offset/delay estimates live in PTP_OFFSET_RAW / PTP_DELAY_NS;
with both clocks free-running on uncalibrated HSI addends the
estimates are dominated by relative drift over the round (3-90 us
observed) rather than the sub-microsecond cable delay -- exactly the
error the milestone-10 servo exists to remove.

Two compiler/model notes from this milestone: the @extent(buf1)
obligation reads the whole DES2 word as a byte count, so the TTSE
control bit makes a definite false violation (masked/sub-field extents
recorded as a model gap); and a bare `[0, ...]` array literal in a
struct-field position infers i32 elements and trips opt (typed-const
workaround; recorded compiler nit).

### 10. Minimal PTP Slave Servo - Done (HSI-Bounded)

After timestamp exchange works:

- Estimate offset and path delay on the mic node.
- First expose offset/drift only.
- Then adjust the local ETH PTP clock offset/addend.
- Keep a lock/state estimate for health reporting.

Success criteria:

- Mic node converges toward controller PTP time.
- Health/status frames report current offset estimate and lock state.

Status (2026-06-12): DONE within the HSI clock's physics. Step-then-slew
servo in main_mic_node.bml on the ptp_clock.bml primitives (ptp_step =
coarse TSUPDT add/sub, ptp_set_addend = TSAR rate discipline): the first
round coarse-steps by t2-t1, divergence > 100 ms drops lock and
re-steps (validated live -- reflashing the controller reset the master
clock and the mic re-stepped automatically, STEPS=3 across the session),
and each accepted round applies an INCREMENTAL frequency + phase update
(full deadbeat on the offset delta -- a direct rate-residual measurement
-- plus off/2 phase slew). A delay sanity gate rejects rounds with
implausible path delay. Measured end state: LOCK=2 sustained 74+ rounds,
offsets mean -69 us / sigma 187 us / max 457 us, path delay rock-steady
at 1.1 us (sigma ~30 ns). The controller records the mic's reported
lock/offset from HEALTH_STATUS (STATUS_LAST_LOCK/PTPOFF) -- the health
criterion verified through the other board.

The servo journey, recorded because each wrong turn was measured: a
base-anchored PI oscillated +-1-2.5 ms (changing the addend is rate
control, so P-on-addend is already integral action in the phase domain);
the mic's delay()-paced loop coupled rate error into the measurement
(offset inflates by rate x turnaround/2 -- positive feedback with the
servo in the loop; fixed by the tight loop with PTP-paced heartbeats);
controller TX bursts overflowed the mic's 4-deep ring (TSENALL doubles
descriptor cost; fixed by staggering to <= 2 frames per tick); and the
16-round probe ring (PTP_LOG_OFF/DLY) finally separated drift from noise
-- a smooth accelerating ramp at a 30 ns-sigma delay floor, i.e. the HSI
RC oscillator thermally wandering at hundreds of ppm. Full deadbeat
tracks it boundedly; half-gain damping lost the race (+750 us trend).
The +-200 us-class residual IS the HSI floor: the crystal-fed clock tree
(HSE via the ST-Link MCO) is the real fix and stays listed as the next
clock milestone before audio-grade sync claims.

### 10b. Clock Tree (HSE/PLL1) -- Done, with a falsified premise

Status: Done (2026-06-11). `clocks.bml` brings up HSE (bypass, the
ST-LINK MCO) -> PLL1 (DIVM1=1, x32, /4) -> the SAME 64 MHz SYSCLK the
HSI delivered, so no flash-latency/VOS change and the TIM2/PTP_SSINC
math is untouched; with a true 64 MHz reference PTP_ADDEND=0xC8000000
is exact by construction. Every stage is readback-gated with a bounded
spin and reports into a SWD-visible CLK_STATUS instead of hanging.
Both boards: CLK_STATUS=4, RCC.CFGR SWS=PLL1 confirmed live.

THE PREMISE WAS WRONG, and measuring that was the milestone's real
yield. The plan assumed the ST-LINK MCO is crystal-derived (UM2407
suggests as much). Measured against NTP wall clock via low-latency SWD
sampling (openocd server + socket, ~1 ms read jitter):

- board A reads +1.4..+1.9e3 ppm, board B +7.4..+7.8e3 ppm -- RC-class
  ACCURACY (a crystal is never 1600 ppm off; consistent with the
  ST-LINK F723's HSI16/2, not its 25 MHz USB crystal);
- consecutive 60 s windows scatter by 100-300 ppm on BOTH boards
  (measurement noise ~25 ppm) -- RC-class STABILITY. The wander, not
  the offset, is what bounds PTP: no servo can remove rate wander that
  accrues within a sync interval.

Bugs the new clock exposed (each pinned by a live-register experiment):

- `ptp_step` subtract encoding: "STSUR = 2^32-s-1" subtracts ONE SECOND
  TOO MUCH. SWD pokes on a live clock: asked -100 ms, got -1.097 s;
  with `2^32-s` got -97 ms exact (hardware handles the ns borrow).
  Masked in milestone 10 because every overshot step was followed by a
  corrective +1 s step (the "STEPS=3" session was really step pairs).
- Addend bookkeeping across re-steps: resetting PTP_ADDEND_NOW to base
  on a step made the first post-step servo write yank the hardware
  addend back to ~nominal, re-creating the full drift. A step is a
  phase event; the learned rate must survive it.
- Servo authority clamp +-16e6 (~4768 ppm) could not reach the real
  ~5600 ppm board-to-board rate difference -- the addend railed and
  offsets ramped to the divergence gate forever. Widened to +-70e6
  (covers two +-1% RC references). TSAR-halving over SWD (rate measured
  0.501x) proved the addend path itself was healthy before widening.

Servo regime change: the milestone-10 full deadbeat, the right answer
to a smoothly wandering on-die HSI, limit-cycled at sigma 332 us
against the new reference while path delay sat at 28 ns sigma -- pure
servo dynamics. Replaced with a two-state PI (rate integral
`servo_freq` + non-accumulated proportional term), tuned critically
damped from the plant gain G = 0.298 ppb/LSB x sync interval. The
noise budget is inverted from a normal PTP setup: measurement noise is
~30 ns while the reference random-walks ~200 ppm/min, so the loop
chases every measurement.

With wander dominant the floor is rate-wander x sync-interval, and the
Sync cadence became the lever (TIM2 ARR 1000 -> 250, Sync every 2nd
tick), confirmed by the scaling:

| sync interval | servo            | sigma   | max     |
|---------------|------------------|---------|---------|
| 1.5 s         | deadbeat (M10)   | 332 us  | 676 us  |
| 0.75 s        | PI Kp=3 Ki=1/2   | 225 us  | 463 us  |
| 0.376 s       | PI Kp=6 Ki=1     | 84 us   | 238 us  |

End state: LOCK=2, STREAK 220+, STEPS=1, REJECTS=0, lock window
tightened 500 us -> 250 us (~3 sigma). Honest conclusion: stock
NUCLEO-H723ZG cannot do better than the ~100 us class regardless of
clock tree, because BOTH available references (on-die HSI, ST-LINK
MCO) are RC; audio-grade sync (~20 us = one 48 kHz sample) needs the
unpopulated X3 crystal or an external clock. The PLL bring-up itself
is correct, validated, and required for that future hardware anyway.

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

## HardFault Recorder

`fault.bml` binds the HardFault vector slot (by function name, slot 3) and
records the fault state into probe-visible statics, then parks WITHOUT
growing the stack so the exception frame stays recoverable:

| Static       | Meaning                                                  |
|--------------|----------------------------------------------------------|
| `FAULT_WHO`  | 0 clean; `0xDEADFA17` = recorder fired                   |
| `FAULT_VECT` | ICSR.VECTACTIVE (3 = HardFault)                          |
| `FAULT_CFSR` | raw CFSR (bit 10 IMPRECISERR, bit 9 PRECISERR)           |
| `FAULT_HFSR` | raw HFSR (`0x40000000` = FORCED escalation)              |
| `FAULT_BFAR` | bus fault address (valid only if CFSR bit 15)            |
| `FAULT_MMFAR`| memmanage address (valid only if CFSR bit 7)             |
| `FAULT_MSP`  | MSP at handler entry                                     |

Workflow: get addresses with `llvm-nm <role>.elf | grep FAULT_`, poll
`FAULT_WHO` over openocd; on a hit, `mdw <FAULT_MSP> 16` recovers the
stacked frame -- the pc/xpsr pair is marked by xpsr bit 24 (Thumb). For an
IMPRECISE fault the stacked pc is a skid a few instructions past the
faulting store; strongly-ordered/PPB accesses (e.g. DWT reads) drain the
write buffer, so skids tend to land just after them regardless of where
the bad write was issued.

The recorder was validated by injection: a deliberate buffered store to the
unclocked FMC region at a known tick reproduced the exact production
signature (CFSR=0x0400, HFSR=FORCED) and the stacked pc localized the
poison store to an 11-instruction skid.

## Finding: Posted Tail-Pointer Write Bus Fault - Fixed

Symptom: rare imprecise BusFault (CFSR=0x0400, escalated to HardFault),
first seen once at tick 96 on the controller; became reproducible within
seconds once the bench consumed full frames under a broadcast flood.

Bisection (each variant flood-tested on the board):

| Variant                              | Result | Theory killed            |
|--------------------------------------|--------|--------------------------|
| Poll + rearm, no consume             | clean  | rearm machinery alone    |
| Consume 16 B/frame                   | clean  |                          |
| Consume 256 B/frame                  | dies   |                          |
| Consume bytes 0..127 only            | clean  | -                        |
| Consume bytes 128..255 only          | clean  | deep-offset / FCS reads  |
| Consume 0..255 (both halves)         | dies   | => duration, not offset  |
| No TIM2 ISR at all                   | dies   | ISR involvement          |
| 4-descriptor ring                    | dies   | ring starvation          |
| ETH regs strongly-ordered (MPU)      | clean  | => buffered-write class  |
| `dsb` after tail writes only         | clean  | CONFIRMED FIX            |

Root cause: the write to `DMACRxDTPR`/`DMACTxDTPR` is a posted (buffered)
Device write. With long CPU read bursts keeping the bus busy, the posted
write could stay in flight and complete with an error response, surfacing
as an imprecise BusFault unrelated to the executing instruction. The fix is
a `dsb` immediately after each tail-pointer write -- completion, not just
ordering (`dmb` alone is not enough). The barrier is now DERIVED: the
compiler emits the `dsb` after every store to a declared handoff register
(DMACRx/TxDTPR are handoffs in stm32h723.target), so the driver carries no
hand-written completion barrier. Falsified on the board: the derived-only
build ran 2,049 frames flood-clean where the barrier-less driver died
within 24. Validated: bench 2,162 frames and controller 977 frames
flood-clean where the unfixed driver died within 24 frames.

Negative knowledge worth keeping:

- RAMECC2 monitor flags (0x48023024 = 0x3) latch on CLEAN runs too -- they
  are noise from the reset handler's byte-wise `.bss` zeroing RMW-ing
  ECC-uninitialized SRAM1 words, not a fault signal. Do not chase them.
- The ETH `dma_shared` statics are still word-zeroed before first use
  (`eth_zero_ecc`): the ECC fault theory was falsified, but the zeroing
  removes that diagnostic noise and is correct hygiene on ECC RAM.
- ETH DMACSR showed no fatal-bus-error bit through all of this: the failing
  transaction was always the CPU's, never the DMA master's.

Instrument gotchas (cost real time):

- `llvm-objdump` on LINKED elves silently decodes wide Thumb-2 as
  `<unknown>` -- pass `--triple=thumbv7em-none-eabi`.
- This openocd build drops commands chained after `init` in a single `-c`
  string; pass each command as its own `-c` flag.
- The Mac's link-local address on the test interface can rebind after the
  board resets bounce the link; bind the flood sender with `IP_BOUND_IF`
  (option 25) instead of a fixed source address.

## RX Consumption Bench

`main_bench.bml` measures two disciplines for the same job ("consume a
received frame safely"), alternating per data frame over the same live
traffic:

- DEF leg (defensive idiom): `cpsid` around the whole consumption + copy
  into a 512 B staging buffer + parse the copy.
- BML leg: parse in place, ZERO masking -- the OWN-bit guard the agent
  model already checks is the safety proof.

TIM2 runs at 5 kHz as the innocent-ISR jitter probe: its entry latency is
TIM2.CNT at ISR entry (PSC=0, 15.6 ns units), bucketed by whether a DEF
window was open. Cycle counts per leg come from DWT.CYCCNT (`dwt.bml`).

Build (build.sh only knows controller|mic_node):

```sh
../../../target/debug/bml build --target stm32h723zg.target main_bench.bml
ld.lld -T main_bench.ld main_bench.o -o main_bench.elf
openocd -f interface/stlink.cfg -f target/stm32h7x.cfg   -c "program main_bench.elf verify reset exit"
```

Traffic (board MAC is promiscuous; any broadcast works):

```python
import socket, time
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
s.setsockopt(socket.IPPROTO_IP, 25, socket.if_nametoindex('en8'))
payload = bytes(range(256)) + bytes(160)   # ~460 B frames on the wire
while True:
    s.sendto(payload, ('169.254.255.255', 47777)); time.sleep(0.05)
```

Probe statics: `BENCH_DEF_{FRAMES,CYC_SUM,CYC_MAX,LAST}` /
`BENCH_BML_*` (eth_dma.bml), `BENCH_LAT_DEF` / `BENCH_LAT_OTHER`
(timer.bml). Addresses via `llvm-nm main_bench.elf | grep BENCH`.

Three disciplines, alternating per frame: DEF (global lock + copy to
staging + parse the copy), PTR (in-place parse through the raw agent
pointer -- volatile loads, the sound boundary tool), and VIEW (in-place
parse through a `reclaim` view justified by the rx channel's declared
`completes_by` flag, DMACSR.RI -- inside the window the agent is
excluded, so the reads are plain non-volatile loads the optimizer may
hoist and combine).

Measured (64 MHz HSI, ~460 B frames, 767 frames per leg in one 2-minute
flood, 2026-06-11, agent-pointer volatile lowering active):

| Metric                   | DEF (lock+copy) | PTR (volatile) | VIEW (reclaim) |
|--------------------------|-----------------|----------------|----------------|
| Avg cycles/frame         | 4,943           | 4,617 (-7%)    | 4,262 (-14%)   |
| Max cycles/frame         | 5,133           | 5,009          | 4,530          |
| Extra RAM                | +512 B          | 0              | 0              |
| Max innocent-ISR latency | 76.2 us         | 0.8 us         | 0.8 us (92x)   |
| Masking on payload path  | whole window    | none           | none           |

The triangulation that proves the model's claim: the VIEW leg (4,262)
matches the PRE-volatile raw-pointer number (4,276, within 0.3%) -- the
window-justified path recovers the entire volatile cost, by proof
instead of by luck. Volatile at the unguarded boundary, optimizable
inside the proof.

Getting DMACSR.RI to latch took two pieces of EQOS physics, both now in
the driver: the status bit is gated by its DMACIER enable (RIE+NIE set,
NVIC line stays off -- status only), and it only latches for frames
whose RX descriptor was armed with the read-format IOC bit (RDES3 bit
30; the write-back format reuses that bit as CTXT). The rx channel's
`completes_by = Ethernet_DMA.DMACSR.RI` is declared in
stm32h723.target.

Caveats: both legs are compiled by bml, so this isolates the DISCIPLINE
cost (lock scope + duplicate buffer), not compiler codegen quality; an
expert-C leg (same algorithm, clang -O2, hand-placed BASEPRI) is future
work and a different question. The per-byte accessor call (`rx_get8`)
inflates both legs' absolute numbers equally.

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
