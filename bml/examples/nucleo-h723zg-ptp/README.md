# NUCLEO-H723ZG PTP Bring-Up

First bring-up example for the STM32H723ZG Ethernet PTP hardware on the
NUCLEO-H723ZG board.

The initial test is a raw Ethernet heartbeat frame. It does not use IP, UDP,
ARP, or `ptpd` yet.

See [`BRINGUP.md`](./BRINGUP.md) for the implementation plan and scope.

## Roles

The example builds into two roles for the N:1 product network, sharing the same
target, SVD, PHY, ETH DMA, PTP, and board bring-up code:

- `controller` (`main_controller.bml`): switch/controller stand-in, board id 1.
- `mic_node` (`main_mic_node.bml`): mic-board stand-in, board id 2.

Both currently send the same raw heartbeat and differ only by the board id byte
in the payload. Flash `controller` to one NUCLEO and `mic_node` to the other.

## Build

```sh
./build.sh controller
./build.sh mic_node
```

## Flash

```sh
./flash.sh controller   # board A
./flash.sh mic_node     # board B
```

## Mac-Side Check

Connect the Nucleo Ethernet jack to the Mac, then run:

```sh
sudo tcpdump -ni <iface> -e -xx 'ether proto 0x88b5'
```

Expected result: one broadcast frame per second from `02:00:00:00:72:3a`. Both
roles still share this source MAC for now; tell them apart by the board id byte
at payload offset 32 (`01` controller, `02` mic node). Per-board MACs arrive with
the milestone 8 protocol header.

## Notes

- The example uses D2 SRAM at `0x30000000` as RAM so ETH DMA can access BML
  statics and packet buffers.
- The first version keeps D-cache disabled to avoid DMA coherency issues during
  bring-up.
- Generated SVD files are committed under `svd/` on purpose.
