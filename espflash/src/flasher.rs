use bytemuck::{__core::time::Duration, bytes_of, Pod, Zeroable};
use indicatif::{ProgressBar, ProgressStyle};
use serial::{BaudRate, SerialPort};
use strum_macros::Display;

use std::{mem::size_of, thread::sleep};

use crate::{
    chip::Chip, connection::Connection, elf::FirmwareImage, encoder::SlipEncoder, error::RomError,
    Error, PartitionTable,
};

type Encoder<'a> = SlipEncoder<'a, Box<dyn SerialPort>>;

const MAX_RAM_BLOCK_SIZE: usize = 0x1800;
const FLASH_SECTOR_SIZE: usize = 0x1000;
const FLASH_BLOCK_SIZE: usize = 0x100;
const FLASH_SECTORS_PER_BLOCK: usize = FLASH_SECTOR_SIZE / FLASH_BLOCK_SIZE;
const FLASH_WRITE_SIZE: usize = 0x400;

// register used for chip detect
const CHIP_DETECT_MAGIC_REG_ADDR: u32 = 0x40001000;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const ERASE_REGION_TIMEOUT_PER_MB: Duration = Duration::from_secs(30);
const ERASE_WRITE_TIMEOUT_PER_MB: Duration = Duration::from_secs(40);
const MEM_END_TIMEOUT: Duration = Duration::from_millis(50);
const SYNC_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Copy, Clone, Debug)]
#[allow(dead_code)]
#[repr(u8)]
enum Command {
    FlashBegin = 0x02,
    FlashData = 0x03,
    FlashEnd = 0x04,
    MemBegin = 0x05,
    MemEnd = 0x06,
    MemData = 0x07,
    Sync = 0x08,
    WriteReg = 0x09,
    ReadReg = 0x0a,
    SpiSetParams = 0x0B,
    SpiAttach = 0x0D,
    ChangeBaud = 0x0F,
}

impl Command {
    pub fn timeout(&self) -> Duration {
        match self {
            Command::MemEnd => MEM_END_TIMEOUT,
            Command::Sync => SYNC_TIMEOUT,
            _ => DEFAULT_TIMEOUT,
        }
    }

    pub fn timeout_for_size(&self, size: u32) -> Duration {
        fn calc_timeout(timeout_per_mb: Duration, size: u32) -> Duration {
            let mb = size as f64 / 1_000_000.0;
            std::cmp::max(
                DEFAULT_TIMEOUT,
                Duration::from_millis((timeout_per_mb.as_millis() as f64 * mb) as u64),
            )
        }
        match self {
            Command::FlashBegin => calc_timeout(ERASE_REGION_TIMEOUT_PER_MB, size),
            Command::FlashData => calc_timeout(ERASE_WRITE_TIMEOUT_PER_MB, size),
            _ => self.timeout(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Display)]
#[allow(dead_code)]
#[repr(u8)]
pub enum FlashSize {
    #[strum(serialize = "256KB")]
    Flash256Kb = 0x12,
    #[strum(serialize = "512KB")]
    Flash512Kb = 0x13,
    #[strum(serialize = "1MB")]
    Flash1Mb = 0x14,
    #[strum(serialize = "2MB")]
    Flash2Mb = 0x15,
    #[strum(serialize = "4MB")]
    Flash4Mb = 0x16,
    #[strum(serialize = "8MB")]
    Flash8Mb = 0x17,
    #[strum(serialize = "16MB")]
    Flash16Mb = 0x18,
    FlashRetry = 0xFF, // used to hint that alternate detection should be tried
}

impl FlashSize {
    fn from(value: u8) -> Result<FlashSize, Error> {
        match value {
            0x12 => Ok(FlashSize::Flash256Kb),
            0x13 => Ok(FlashSize::Flash512Kb),
            0x14 => Ok(FlashSize::Flash1Mb),
            0x15 => Ok(FlashSize::Flash2Mb),
            0x16 => Ok(FlashSize::Flash4Mb),
            0x17 => Ok(FlashSize::Flash8Mb),
            0x18 => Ok(FlashSize::Flash16Mb),
            0xFF => Ok(FlashSize::FlashRetry),
            _ => Err(Error::UnsupportedFlash(value)),
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
struct SpiAttachParams {
    clk: u8,
    q: u8,
    d: u8,
    hd: u8,
    cs: u8,
}

impl SpiAttachParams {
    pub const fn default() -> Self {
        SpiAttachParams {
            clk: 0,
            q: 0,
            d: 0,
            hd: 0,
            cs: 0,
        }
    }

    pub const fn esp32_pico_d4() -> Self {
        SpiAttachParams {
            clk: 6,
            q: 17,
            d: 8,
            hd: 11,
            cs: 16,
        }
    }

    pub fn encode(self) -> Vec<u8> {
        let packed = ((self.hd as u32) << 24)
            | ((self.cs as u32) << 18)
            | ((self.d as u32) << 12)
            | ((self.q as u32) << 6)
            | (self.clk as u32);
        if packed == 0 {
            vec![0; 5]
        } else {
            packed.to_le_bytes().to_vec()
        }
    }
}

/// List of spi params to try while detecting flash size
const TRY_SPI_PARAMS: [SpiAttachParams; 2] =
    [SpiAttachParams::default(), SpiAttachParams::esp32_pico_d4()];

#[derive(Zeroable, Pod, Copy, Clone, Debug)]
#[repr(C)]
struct BlockParams {
    size: u32,
    sequence: u32,
    dummy1: u32,
    dummy2: u32,
}

#[derive(Zeroable, Pod, Copy, Clone, Debug)]
#[repr(C)]
struct BeginParams {
    size: u32,
    blocks: u32,
    block_size: u32,
    offset: u32,
    encrypted: u32,
}

#[derive(Zeroable, Pod, Copy, Clone, Debug)]
#[repr(C)]
struct WriteRegParams {
    addr: u32,
    value: u32,
    mask: u32,
    delay_us: u32,
}

#[derive(Zeroable, Pod, Copy, Clone)]
#[repr(C)]
struct EntryParams {
    no_entry: u32,
    entry: u32,
}

pub struct Flasher {
    connection: Connection,
    chip: Chip,
    flash_size: FlashSize,
    spi_params: SpiAttachParams,
}

impl Flasher {
    pub fn connect(
        serial: impl SerialPort + 'static,
        speed: Option<BaudRate>,
    ) -> Result<Self, Error> {
        let mut flasher = Flasher {
            connection: Connection::new(serial), // default baud is always 115200
            chip: Chip::Esp8266,                 // dummy, set properly later
            flash_size: FlashSize::Flash4Mb,
            spi_params: SpiAttachParams::default(), // may be set when trying to attach to flash
        };
        flasher.start_connection()?;
        flasher.connection.set_timeout(DEFAULT_TIMEOUT)?;
        flasher.chip_detect()?;
        flasher.spi_autodetect()?;

        if let Some(b) = speed {
            match flasher.chip {
                Chip::Esp8266 => (), /* Not available */
                _ => {
                    if b.speed() > BaudRate::Baud115200.speed() {
                        println!("WARN setting baud rate higher than 115200 can cause issues.");
                        flasher.change_baud(b)?;
                    }
                }
            }
        }

        Ok(flasher)
    }

    fn spi_autodetect(&mut self) -> Result<(), Error> {
        // loop over all available spi params until we find one that successfully reads
        // the flash size
        for spi_params in TRY_SPI_PARAMS.iter().copied() {
            self.enable_flash(spi_params)?;
            if self.flash_detect()? {
                // flash detect successful, save these spi params
                self.spi_params = spi_params;
                return Ok(());
            }
        }

        // none of the spi parameters were successful
        Err(Error::UnsupportedFlash(FlashSize::FlashRetry as u8))
    }

    fn chip_detect(&mut self) -> Result<(), Error> {
        let magic = self.read_reg(CHIP_DETECT_MAGIC_REG_ADDR)?;
        let chip = Chip::from_magic(magic).ok_or(Error::UnrecognizedChip)?;

        self.chip = chip;
        Ok(())
    }

    fn flash_detect(&mut self) -> Result<bool, Error> {
        let flash_id = self.spi_command(0x9f, &[], 24)?;
        let size_id = flash_id >> 16;

        self.flash_size = FlashSize::from(size_id as u8)?;
        Ok(self.flash_size != FlashSize::FlashRetry)
    }

    fn sync(&mut self) -> Result<(), Error> {
        self.connection
            .with_timeout(Command::Sync.timeout(), |connection| {
                let data = &[
                    0x07, 0x07, 0x12, 0x20, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
                    0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
                    0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55, 0x55,
                ][..];

                connection.write_command(Command::Sync as u8, data, 0)?;

                for _ in 0..100 {
                    match connection.read_response()? {
                        Some(response) if response.return_op == Command::Sync as u8 => {
                            if response.status == 1 {
                                let _error = connection.flush();
                                return Err(Error::RomError(RomError::from(response.error)));
                            } else {
                                break;
                            }
                        }
                        _ => continue,
                    }
                }

                Ok(())
            })?;
        for _ in 0..700 {
            match self.connection.read_response()? {
                Some(_) => break,
                _ => continue,
            }
        }
        Ok(())
    }

    fn start_connection(&mut self) -> Result<(), Error> {
        self.connection.reset_to_flash()?;
        for _ in 0..10 {
            self.connection.flush()?;
            if self.sync().is_ok() {
                return Ok(());
            }
        }
        Err(Error::ConnectionFailed)
    }

    fn begin_command(
        &mut self,
        command: Command,
        size: u32,
        blocks: u32,
        block_size: u32,
        offset: u32,
    ) -> Result<(), Error> {
        let params = BeginParams {
            size,
            blocks,
            block_size,
            offset,
            encrypted: 0,
        };

        let bytes = bytes_of(&params);
        let data = if self.chip == Chip::Esp32 || self.chip == Chip::Esp8266 {
            // The ESP32 and ESP8266 do not take the `encrypted` field, so truncate the last
            // 4 bytes of the slice where it resides.
            let end = bytes.len() - 4;
            &bytes[0..end]
        } else {
            bytes
        };

        self.connection
            .with_timeout(command.timeout_for_size(size), |connection| {
                connection.command(command as u8, data, 0)?;
                Ok(())
            })
    }

    fn block_command(
        &mut self,
        command: Command,
        data: &[u8],
        padding: usize,
        padding_byte: u8,
        sequence: u32,
    ) -> Result<(), Error> {
        let params = BlockParams {
            size: (data.len() + padding) as u32,
            sequence,
            dummy1: 0,
            dummy2: 0,
        };

        let length = size_of::<BlockParams>() + data.len() + padding;

        let mut check = checksum(data, CHECKSUM_INIT);

        for _ in 0..padding {
            check = checksum(&[padding_byte], check);
        }

        self.connection
            .with_timeout(command.timeout_for_size(data.len() as u32), |connection| {
                connection.command(
                    command as u8,
                    (length as u16, |encoder: &mut Encoder| {
                        encoder.write(bytes_of(&params))?;
                        encoder.write(data)?;
                        let padding = &[padding_byte; FLASH_WRITE_SIZE][0..padding];
                        encoder.write(padding)?;
                        Ok(())
                    }),
                    check as u32,
                )?;
                Ok(())
            })
    }

    fn mem_finish(&mut self, entry: u32) -> Result<(), Error> {
        let params = EntryParams {
            no_entry: (entry == 0) as u32,
            entry,
        };
        self.connection
            .with_timeout(Command::MemEnd.timeout(), |connection| {
                connection.write_command(Command::MemEnd as u8, bytes_of(&params), 0)
            })
    }

    fn flash_finish(&mut self, reboot: bool) -> Result<(), Error> {
        self.connection
            .with_timeout(Command::FlashEnd.timeout(), |connection| {
                connection.write_command(Command::FlashEnd as u8, &[(!reboot) as u8][..], 0)
            })
    }

    fn enable_flash(&mut self, spi_attach_params: SpiAttachParams) -> Result<(), Error> {
        match self.chip {
            Chip::Esp8266 => {
                self.begin_command(Command::FlashBegin, 0, 0, FLASH_WRITE_SIZE as u32, 0)?;
            }
            _ => {
                let spi_params = spi_attach_params.encode();
                self.connection
                    .with_timeout(Command::SpiAttach.timeout(), |connection| {
                        connection.command(Command::SpiAttach as u8, spi_params.as_slice(), 0)
                    })?;
            }
        }
        Ok(())
    }

    fn spi_command(&mut self, command: u8, data: &[u8], read_bits: u32) -> Result<u32, Error> {
        assert!(read_bits < 32);
        assert!(data.len() < 64);

        let spi_registers = self.chip.spi_registers();

        let old_spi_usr = self.read_reg(spi_registers.usr())?;
        let old_spi_usr2 = self.read_reg(spi_registers.usr2())?;

        let mut flags = 1 << 31;
        if !data.is_empty() {
            flags |= 1 << 27;
        }
        if read_bits > 0 {
            flags |= 1 << 28;
        }

        self.write_reg(spi_registers.usr(), flags, None)?;
        self.write_reg(spi_registers.usr2(), 7 << 28 | command as u32, None)?;

        if let (Some(mosi_data_length), Some(miso_data_length)) =
            (spi_registers.mosi_length(), spi_registers.miso_length())
        {
            if !data.is_empty() {
                self.write_reg(mosi_data_length, data.len() as u32 * 8 - 1, None)?;
            }
            if read_bits > 0 {
                self.write_reg(miso_data_length, read_bits - 1, None)?;
            }
        } else {
            let mosi_mask = if data.is_empty() {
                0
            } else {
                data.len() as u32 * 8 - 1
            };
            let miso_mask = if read_bits == 0 { 0 } else { read_bits - 1 };
            self.write_reg(spi_registers.usr1(), miso_mask << 8 | mosi_mask << 17, None)?;
        }

        if data.is_empty() {
            self.write_reg(spi_registers.w0(), 0, None)?;
        } else {
            for (i, bytes) in data.chunks(4).enumerate() {
                let mut data_bytes = [0; 4];
                data_bytes[0..bytes.len()].copy_from_slice(bytes);
                let data = u32::from_le_bytes(data_bytes);
                self.write_reg(spi_registers.w0() + i as u32, data, None)?;
            }
        }

        self.write_reg(spi_registers.cmd(), 1 << 18, None)?;

        let mut i = 0;
        loop {
            sleep(Duration::from_millis(1));
            if self.read_reg(spi_registers.usr())? & (1 << 18) == 0 {
                break;
            }
            i += 1;
            if i > 10 {
                return Err(Error::Timeout);
            }
        }

        let result = self.read_reg(spi_registers.w0())?;
        self.write_reg(spi_registers.usr(), old_spi_usr, None)?;
        self.write_reg(spi_registers.usr2(), old_spi_usr2, None)?;

        Ok(result)
    }

    fn read_reg(&mut self, reg: u32) -> Result<u32, Error> {
        self.connection
            .with_timeout(Command::ReadReg.timeout(), |connection| {
                connection.command(Command::ReadReg as u8, &reg.to_le_bytes()[..], 0)
            })
    }

    fn write_reg(&mut self, addr: u32, value: u32, mask: Option<u32>) -> Result<(), Error> {
        let params = WriteRegParams {
            addr,
            value,
            mask: mask.unwrap_or(0xFFFFFFFF),
            delay_us: 0,
        };
        self.connection
            .with_timeout(Command::WriteReg.timeout(), |connection| {
                connection.command(Command::WriteReg as u8, bytes_of(&params), 0)
            })?;
        Ok(())
    }

    /// The chip type that the flasher is connected to
    pub fn chip(&self) -> Chip {
        self.chip
    }

    /// The flash size of the board that the flasher is connected to
    pub fn flash_size(&self) -> FlashSize {
        self.flash_size
    }

    /// Load an elf image to ram and execute it
    ///
    /// Note that this will not touch the flash on the device
    pub fn load_elf_to_ram(&mut self, elf_data: &[u8]) -> Result<(), Error> {
        let image = FirmwareImage::from_data(elf_data).map_err(|_| Error::InvalidElf)?;

        if image.rom_segments(self.chip).next().is_some() {
            return Err(Error::ElfNotRamLoadable);
        }

        for segment in image.ram_segments(self.chip) {
            let padding = 4 - segment.data.len() % 4;
            let block_count =
                (segment.data.len() + padding + MAX_RAM_BLOCK_SIZE - 1) / MAX_RAM_BLOCK_SIZE;
            self.begin_command(
                Command::MemBegin,
                segment.data.len() as u32,
                block_count as u32,
                MAX_RAM_BLOCK_SIZE as u32,
                segment.addr,
            )?;

            for (i, block) in segment.data.chunks(MAX_RAM_BLOCK_SIZE).enumerate() {
                let block_padding = if i == block_count - 1 { padding } else { 0 };
                self.block_command(Command::MemData, block, block_padding, 0, i as u32)?;
            }
        }

        self.mem_finish(image.entry())?;

        Ok(())
    }

    /// Load an elf image to flash and execute it
    pub fn load_elf_to_flash(
        &mut self,
        elf_data: &[u8],
        bootloader: Option<Vec<u8>>,
        partition_table: Option<PartitionTable>,
    ) -> Result<(), Error> {
        self.enable_flash(self.spi_params)?;

        let mut image = FirmwareImage::from_data(elf_data).map_err(|_| Error::InvalidElf)?;
        image.flash_size = self.flash_size();

        for segment in self
            .chip
            .get_flash_segments(&image, bootloader, partition_table)
        {
            let segment = segment?;
            let addr = segment.addr;
            let block_count = (segment.data.len() + FLASH_WRITE_SIZE - 1) / FLASH_WRITE_SIZE;

            let erase_size = match self.chip {
                Chip::Esp8266 => get_erase_size(addr as usize, segment.data.len()) as u32,
                _ => segment.data.len() as u32,
            };

            self.begin_command(
                Command::FlashBegin,
                erase_size,
                block_count as u32,
                FLASH_WRITE_SIZE as u32,
                addr,
            )?;

            let chunks = segment.data.chunks(FLASH_WRITE_SIZE);

            let (_, chunk_size) = chunks.size_hint();
            let chunk_size = chunk_size.unwrap_or(0) as u64;
            let pb_chunk = ProgressBar::new(chunk_size);
            pb_chunk.set_style(
                ProgressStyle::default_bar()
                    .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")
                    .progress_chars("#>-"),
            );

            for (i, block) in chunks.enumerate() {
                pb_chunk.set_message(format!("segment 0x{:X} writing chunks", addr));
                let block_padding = FLASH_WRITE_SIZE - block.len();
                self.block_command(Command::FlashData, block, block_padding, 0xff, i as u32)?;
                pb_chunk.inc(1);
            }

            pb_chunk.finish_with_message(format!("segment 0x{:X}", addr));
        }

        self.flash_finish(false)?;

        self.connection.reset()?;

        Ok(())
    }

    pub fn change_baud(&mut self, speed: BaudRate) -> Result<(), Error> {
        let new_speed = (speed.speed() as u32).to_le_bytes();
        let old_speed = 0u32.to_le_bytes();

        self.connection
            .with_timeout(Command::ChangeBaud.timeout(), |connection| {
                connection.command(
                    Command::ChangeBaud as u8,
                    &[new_speed, old_speed].concat()[..],
                    0,
                )
            })?;
        self.connection.set_baud(speed)?;
        std::thread::sleep(Duration::from_secs_f32(0.05));
        self.connection.flush()?;
        Ok(())
    }
}

fn get_erase_size(offset: usize, size: usize) -> usize {
    let sector_count = (size + FLASH_SECTOR_SIZE - 1) / FLASH_SECTOR_SIZE;
    let start_sector = offset / FLASH_SECTOR_SIZE;

    let head_sectors = usize::min(
        FLASH_SECTORS_PER_BLOCK - (start_sector % FLASH_SECTORS_PER_BLOCK),
        sector_count,
    );

    if sector_count < 2 * head_sectors {
        (sector_count + 1) / 2 * FLASH_SECTOR_SIZE
    } else {
        (sector_count - head_sectors) * FLASH_SECTOR_SIZE
    }
}

const CHECKSUM_INIT: u8 = 0xEF;

pub fn checksum(data: &[u8], mut checksum: u8) -> u8 {
    for byte in data {
        checksum ^= *byte;
    }

    checksum
}
