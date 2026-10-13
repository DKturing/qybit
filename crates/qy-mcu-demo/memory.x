/* QEMU mps2-an386（Cortex-M4F）：ZBT SSRAM1 0x0000_0000 放代码与“SD 卡”文件，
   SSRAM2/3 0x2000_0000 起取 512 KiB 作 RAM（.data/.bss/栈），接近常见 Cortex-M7 板的片上 SRAM。 */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 4M
  RAM   : ORIGIN = 0x20000000, LENGTH = 512K
}
