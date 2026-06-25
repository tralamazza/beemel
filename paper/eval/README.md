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

Sources for each hazard are cited in the paper (AN4839, the Nordic EasyDMA
product spec, Zephyr issue #36471, NuttX CONFIG_STM32_CCMEXCLUDE, embassy's
BufferNotInRAM, and the ST community threads).
