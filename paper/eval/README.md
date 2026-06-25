# Documented-hazard catalog (Evaluation section, S7.1)

Minimal BML reconstructions of four independently-documented, multi-vendor
Cortex-M DMA hazards. Each `*.target` + `*.bml` pair, run through `bml build`,
is rejected by the compiler. These reconstruct the documented memory-sharing
structure; BML cannot ingest the original C drivers.

Reproduce (no hardware needed; the reach/cache checks run at target load):

    bml build --target h7_dtcm.target  h7_dtcm.bml   # ST H7  DMA-in-DTCM  -> reach reject
    bml build --target f4_ccm.target   f4_ccm.bml    # ST F4  DMA-in-CCM   -> reach reject
    bml build --target nrf_flash.target nrf_flash.bml # nRF   EasyDMA-flash -> reach reject
    bml build --target h7_cache.target h7_dtcm.bml   # H7/SAME70 cacheable -> coherence reject

The fixed counterparts build clean:

    # move the buffer to reachable RAM, or add `cacheable = false` to the block
    sed 's/^mem = dtcm/mem = sram1/' h7_dtcm.target > h7_fixed.target
    bml build --target h7_fixed.target h7_dtcm.bml   # builds through codegen

For the cache case, adding `cacheable = false` to the shared block makes the
build succeed AND emit a non-cacheable MPU region in the generated reset handler
(PMSAv7 RNR/RBAR/RASR stores into 0xE000EDxx).

## Third-party drivers (Evaluation section, S7.2)

Reconstructions anchored to named third-party driver issues (drivers BML never
saw), each rejected by the compiler:

    bml build --target stm32eth_issue16.target stm32eth_issue16.bml
    # stm32-rs/stm32-eth #16: cacheable ETH descriptor ring -> "cache views diverge"
    # (their fix: documented placement requirement + linker section)

    bml build --target nrf_flash.target nrf_flash.bml
    # nrf-rs/nrf-hal #37: EasyDMA/UARTE TX buffer in flash -> reach reject
    # (their fix: a run-time BufferNotInRAM check, or a copy into RAM)

Sources for each hazard are cited in the paper (AN4839, the Nordic EasyDMA
product spec, Zephyr issue #36471, NuttX CONFIG_STM32_CCMEXCLUDE, embassy's
BufferNotInRAM, the stm32-eth #16 and nrf-hal #37 issues, and ST community
threads).

## Cost measurement (Evaluation section, S7.5)

Generated-MPU code size, isolated by toggling `has_mpu` on the same program
(everything else identical, so the `.text` delta is exactly the MPU setup):

    bml build --out-dir /tmp --target cost_mpu.target   cost.bml   # has_mpu = true
    bml build --out-dir /tmp --target cost_nompu.target cost.bml   # has_mpu = false
    llvm-size /tmp/... # .text: 198 (MPU) vs 134 (no MPU) -> 64 bytes, once at reset

Annotation burden is content-line-counted on the example drivers (e.g.
eth_dma.bml: 26 model annotations / 701 SLOC = 3.7%); chip physics is the shipped
lib/stm32h723/stm32h723.target (51 lines of physics, amortized per chip).
