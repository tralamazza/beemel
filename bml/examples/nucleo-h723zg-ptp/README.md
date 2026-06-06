# NUCLEO-H723ZG PTP Bring-Up

First bring-up example for the STM32H723ZG Ethernet PTP hardware on the
NUCLEO-H723ZG board.

The initial test is a raw Ethernet heartbeat frame. It does not use IP, UDP,
ARP, or `ptpd` yet.

See [`BRINGUP.md`](./BRINGUP.md) for the implementation plan and scope.

## Build

```sh
./build.sh
```

## Flash

```sh
./flash.sh
```

## Mac-Side Check

Connect the Nucleo Ethernet jack to the Mac, then run:

```sh
sudo tcpdump -ni <iface> -e -xx 'ether proto 0x88b5'
```

Expected result after ETH TX is implemented: one broadcast frame per second from
`02:00:00:00:72:3a`.

## Notes

- The example uses D2 SRAM at `0x30000000` as RAM so ETH DMA can access BML
  statics and packet buffers.
- The first version keeps D-cache disabled to avoid DMA coherency issues during
  bring-up.
- Generated SVD files are committed under `svd/` on purpose.
