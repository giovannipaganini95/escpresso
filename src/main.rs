use anyhow::Result;
use clap::Parser;
use eframe::egui;
use encoding_rs::Encoding;
use oem_cp::code_table::DECODING_TABLE_CP_MAP;
use qrcode::{Color as QrColor, QrCode};
use rxing::{BarcodeFormat, MultiFormatWriter, Writer};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const ESC: u8 = 0x1B;
const GS: u8 = 0x1D;
const FS: u8 = 0x1C;
const DLE: u8 = 0x10;
const LF: u8 = 0x0A;
const FF: u8 = 0x0C;
const CR: u8 = 0x0D;
const HT: u8 = 0x09;
const CAN: u8 = 0x18;
const DC2: u8 = 0x12;
const SOH: u8 = 0x01;
const STX: u8 = 0x02;
const ETX: u8 = 0x03;
const EOT: u8 = 0x04;
const ENQ: u8 = 0x05;
const ACK: u8 = 0x06;
const BEL: u8 = 0x07;
const BS: u8 = 0x08;
const VT: u8 = 0x0B;
const SO: u8 = 0x0E;
const SI: u8 = 0x0F;
const DC1: u8 = 0x11;
const DC3: u8 = 0x13;
const DC4: u8 = 0x14;
const ETB: u8 = 0x17;
const RS: u8 = 0x1E;

#[derive(Debug, Clone, Copy, PartialEq)]
enum PaperSize {
    Size58mm,
    Size80mm,
}

impl PaperSize {
    fn width_px(&self) -> f32 {
        // Printable area width (print head), not full paper
        // 80mm paper: 72mm print head = 576 dots (48 cols * 12 dots)
        // 58mm paper: 48mm print head = 384 dots (32 cols * 12 dots)
        (self.chars_per_line() as f32) * 12.0
    }

    fn chars_per_line(&self) -> usize {
        match self {
            PaperSize::Size58mm => 32,
            PaperSize::Size80mm => 48,
        }
    }

    fn label(&self) -> &str {
        match self {
            PaperSize::Size58mm => "58mm",
            PaperSize::Size80mm => "80mm",
        }
    }
}

#[derive(Debug, Clone)]
enum ReceiptElement {
    Text {
        content: String,
        bold: bool,
        underline: bool,
        double_width: bool,
        double_height: bool,
        inverted: bool,
        alignment: Alignment,
        density: u8,
        offset: u16,
        left_margin: u16,
        character_spacing: u8,
        double_strike: bool,
        font: u8,
        print_area_width: u16,
    },
    RasterImage {
        width: usize, // Width in pixels (for display)
        height: usize,
        data: Vec<u8>,
        offset: u16,
        density: u8,
        alignment: Alignment,
        bytes_per_line: usize, // Actual bytes per line from command (for data reading)
        print_area_width: u16,
    },
    QrCode {
        data: String,
        size: usize,
        alignment: Alignment,
        offset: u16,
        print_area_width: u16,
    },
    Barcode {
        data: Vec<u8>,
        barcode_type: BarcodeType,
        height: u8,
        module_width: u8,
        hri_position: HriPosition,
        alignment: Alignment,
        offset: u16,
        print_area_width: u16,
    },
    Barcode2D {
        data: Vec<u8>,
        variant: Barcode2DVariant,
        module_size: u8,
        alignment: Alignment,
        offset: u16,
        print_area_width: u16,
    },
    PaperCut {
        cut_type: String,
    },
    CashDrawer {
        pin: u8,
        on_time: u8,
        off_time: u8,
    },
    Separator,
    FormFeed,
}

#[derive(Debug, Clone)]
enum Alignment {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone)]
enum BarcodeType {
    UpcA,
    UpcE,
    Ean13,
    Ean8,
    Code39,
    Itf,
    Codabar,
    Code93,
    Code128,
}

impl BarcodeType {
    fn to_rxing_format(&self) -> BarcodeFormat {
        match self {
            BarcodeType::UpcA => BarcodeFormat::UPC_A,
            BarcodeType::UpcE => BarcodeFormat::UPC_E,
            BarcodeType::Ean13 => BarcodeFormat::EAN_13,
            BarcodeType::Ean8 => BarcodeFormat::EAN_8,
            BarcodeType::Code39 => BarcodeFormat::CODE_39,
            BarcodeType::Itf => BarcodeFormat::ITF,
            BarcodeType::Codabar => BarcodeFormat::CODABAR,
            BarcodeType::Code93 => BarcodeFormat::CODE_93,
            BarcodeType::Code128 => BarcodeFormat::CODE_128,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum HriPosition {
    None,
    Above,
    Below,
    Both,
}

#[derive(Debug, Clone)]
enum Barcode2DVariant {
    Pdf417,
    DataMatrix,
}

#[derive(Debug)]
struct PrinterState {
    bold: bool,
    underline: bool,
    double_width: bool,
    double_height: bool,
    inverted: bool,
    alignment: Alignment,
    print_density: u8,
    encoding: &'static Encoding,
    code_page: u8,
    horizontal_offset: u16,
    left_margin: u16,
    print_area_width: u16,
    line_spacing: u8,
    character_spacing: u8,
    double_strike: bool,
    font: u8, // 0=Font A, 1=Font B, etc.
}

impl Default for PrinterState {
    fn default() -> Self {
        Self {
            bold: false,
            underline: false,
            double_width: false,
            double_height: false,
            inverted: false,
            alignment: Alignment::Left,
            print_density: 4,
            encoding: encoding_rs::UTF_8,
            code_page: 0,
            horizontal_offset: 0,
            left_margin: 0,
            print_area_width: 0, // 0 = use default (full width)
            line_spacing: 30,    // Default: 1/6 inch = ~30 dots at 203 DPI
            character_spacing: 0,
            double_strike: false,
            font: 0, // Default: Font A
        }
    }
}

/// Map an ESC/POS codepage number (from `ESC t n`) to the corresponding
/// Windows codepage number used by `oem_cp::code_table::DECODING_TABLE_CP_MAP`.
///
/// Returns `None` for codepages that are not OEM/DOS pages and should fall
/// through to `encoding_rs` instead (e.g. Windows-1252, Shift-JIS).
fn escpos_to_windows_cp(escpos_cp: u8) -> Option<u16> {
    match escpos_cp {
        0       => Some(437),  // PC437
        2       => Some(850),  // PC850
        3       => Some(860),  // PC860 (Portuguese)
        4       => Some(863),  // PC863 (French-Canadian)
        5       => Some(865),  // PC865 (Nordic)
        14 | 19 => Some(858),  // PC858 (CP850 + €)
        17      => Some(866),  // PC866 (Cyrillic)
        18      => Some(852),  // PC852 (Central European)
        _       => None,
    }
}

struct EscPosRenderer {
    state: PrinterState,
    current_line: Vec<u8>,
    debug: bool,
    buffer: Vec<u8>,
    elements: Vec<ReceiptElement>,
    in_command_sequence: bool,
    qr_data: Vec<u8>,
    qr_size: u8,
    qr_error_correction: u8,
    barcode_hri_position: HriPosition,
    barcode_height: u8,
    barcode_module_width: u8,
    pdf417_data: Vec<u8>,
    pdf417_columns: u8,
    pdf417_rows: u8,
    pdf417_module_width: u8,
    pdf417_error_correction: u8,
    pdf417_truncated: bool,
    datamatrix_data: Vec<u8>,
    datamatrix_module_size: u8,
    response_queue: Vec<u8>,
    last_was_binary: bool,
    printer_status: Arc<Mutex<PrinterStatus>>,
    nv_images: Arc<Mutex<HashMap<u8, NvBitImage>>>,
    command_log: Arc<Mutex<Vec<CommandLogEntry>>>,
}

impl EscPosRenderer {
    fn new(
        debug: bool,
        printer_status: Arc<Mutex<PrinterStatus>>,
        nv_images: Arc<Mutex<HashMap<u8, NvBitImage>>>,
        command_log: Arc<Mutex<Vec<CommandLogEntry>>>,
    ) -> Self {
        Self {
            state: PrinterState::default(),
            current_line: Vec::new(),
            debug,
            buffer: Vec::new(),
            elements: Vec::new(),
            in_command_sequence: false,
            qr_data: Vec::new(),
            qr_size: 3,
            qr_error_correction: 0,
            barcode_hri_position: HriPosition::None,
            barcode_height: 162,
            barcode_module_width: 3,
            pdf417_data: Vec::new(),
            pdf417_columns: 0,
            pdf417_rows: 0,
            pdf417_module_width: 3,
            pdf417_error_correction: 1,
            pdf417_truncated: false,
            datamatrix_data: Vec::new(),
            datamatrix_module_size: 3,
            response_queue: Vec::new(),
            last_was_binary: false,
            printer_status,
            nv_images,
            command_log,
        }
    }

    fn log_debug(&self, msg: &str) {
        if self.debug {
            eprintln!("[DEBUG] {}", msg);
        }
    }

    fn log_command(&self, hex: &str, description: &str) {
        self.command_log.lock().unwrap().push(CommandLogEntry {
            hex: hex.to_string(),
            description: description.to_string(),
        });
    }

    fn take_elements(&mut self) -> Vec<ReceiptElement> {
        std::mem::take(&mut self.elements)
    }

    fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.response_queue)
    }

    fn process_data(&mut self, new_data: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(new_data);

        let mut i = 0;
        let data = self.buffer.clone();

        while i < data.len() {
            let byte = data[i];
            let start_pos = i;

            match byte {
                DLE => {
                    // Enter command sequence - block text accumulation
                    self.in_command_sequence = true;
                    // DLE commands (real-time status, etc.)
                    i += 1;
                    if i >= data.len() {
                        i = start_pos;
                        break;
                    }
                    let subcmd = data[i];
                    i += 1;
                    match subcmd {
                        0x04 | 0x05 => {
                            // DLE EOT, DLE ENQ - real-time status
                            if i < data.len() {
                                let _n = data[i];
                                i += 1;

                                let status = self.printer_status.lock().unwrap();
                                let mut byte: u8 = 0x12; // base: online, paper present
                                if !status.paper_present {
                                    byte &= !0x08; // clear bit 3: paper out
                                }
                                if !status.cover_closed {
                                    byte |= 0x20; // set bit 5: cover open
                                }
                                if !status.online {
                                    byte &= !0x10; // clear bit 4: offline
                                }
                                drop(status);
                                self.response_queue.push(byte);
                                self.log_debug(&format!(
                                    "DLE EOT/ENQ: queued status 0x{:02X}",
                                    byte
                                ));
                            }
                        }
                        0x14 => {
                            // DLE DC4 - real-time commands
                            if i + 1 < data.len() {
                                i += 2;
                            }
                        }
                        _ => {}
                    }
                    // Command processed - allow text accumulation again
                    self.in_command_sequence = false;
                }
                CAN => {
                    // Cancel print data in page mode
                    i += 1;
                }
                DC2 => {
                    // DC2 - Cancel bold OR DC2 # n (print density for zj-58)
                    i += 1;
                    if i < data.len() && data[i] == b'#' {
                        // DC2 # n - Set print density (zj-58 CUPS driver)
                        i += 1;
                        if i < data.len() {
                            let density = data[i];
                            self.state.print_density = (density / 32).min(8); // Map 0-255 to 0-8
                            self.log_debug(&format!("DC2 #: print density={}", density));
                            i += 1;
                        }
                    } else {
                        // Standard DC2 - Cancel bold
                        self.state.bold = false;
                    }
                }
                DC1 => {
                    // DC1 / XON - Device control / flow control
                    i += 1;
                }
                DC3 => {
                    // DC3 / XOFF - Device control / flow control
                    i += 1;
                }
                DC4 => {
                    // DC4 - Device control (standalone, not DLE DC4)
                    i += 1;
                }
                SO => {
                    // SO - Shift Out (alternate character set)
                    i += 1;
                }
                SI => {
                    // SI - Shift In (standard character set)
                    i += 1;
                }
                VT => {
                    // VT - Vertical tab
                    i += 1;
                }
                SOH | STX | ETX | EOT | ENQ | ACK | BEL | ETB | RS => {
                    // Other control characters - just skip
                    i += 1;
                }
                BS => {
                    // Backspace - remove last byte if present
                    if !self.current_line.is_empty() {
                        self.current_line.pop();
                    }
                    i += 1;
                }
                ESC => {
                    self.in_command_sequence = true;
                    i += 1;
                    if i >= data.len() {
                        i = start_pos;
                        break;
                    }
                    let cmd_byte = data[i];
                    match self.handle_esc_command(&data, i) {
                        Ok(new_i) => {
                            if new_i == i || new_i <= start_pos {
                                i = start_pos;
                                break;
                            }
                            self.log_command(
                                &format!("1B {:02X}", cmd_byte),
                                &format!("ESC {}", cmd_byte as char),
                            );
                            i = new_i;
                            self.in_command_sequence = false;
                        }
                        Err(e) => return Err(e),
                    }
                }
                GS => {
                    self.in_command_sequence = true;
                    i += 1;
                    if i >= data.len() {
                        i = start_pos;
                        break;
                    }
                    let cmd_byte = data[i];
                    match self.handle_gs_command(&data, i) {
                        Ok(new_i) => {
                            if new_i == i || new_i <= start_pos {
                                i = start_pos;
                                break;
                            }
                            self.log_command(
                                &format!("1D {:02X}", cmd_byte),
                                &format!("GS {}", cmd_byte as char),
                            );
                            i = new_i;
                            self.in_command_sequence = false;
                        }
                        Err(e) => return Err(e),
                    }
                }
                FS => {
                    // Enter command sequence - block text accumulation
                    self.in_command_sequence = true;
                    i += 1;
                    if i >= data.len() {
                        i = start_pos;
                        break;
                    }
                    // FS command handling - many commands have unknown parameter counts
                    let cmd = data[i];
                    i += 1;
                    match cmd {
                        b'.' => {
                            // FS . n - Print NV bit image - 1 parameter
                            // Don't consume parameter if next byte is a command start
                            if i < data.len() {
                                let next = data[i];
                                // Only consume if not a command byte (ESC/GS/FS/DLE)
                                if next != ESC && next != GS && next != FS && next != DLE {
                                    i += 1;
                                }
                            }
                        }
                        b'p' => {
                            // FS p n m - Print NV bit image
                            if i + 1 < data.len() {
                                let slot = data[i];
                                let _mode = data[i + 1];
                                i += 2;

                                let img_clone = self
                                    .nv_images
                                    .lock()
                                    .unwrap()
                                    .get(&slot)
                                    .cloned();

                                if let Some(img) = img_clone {
                                    if !self.current_line.is_empty() {
                                        self.flush_line();
                                        self.current_line.clear();
                                    }
                                    let bytes_per_line = img.width.div_ceil(8);
                                    self.elements.push(ReceiptElement::RasterImage {
                                        width: img.width,
                                        height: img.height,
                                        data: img.data,
                                        offset: self.state.horizontal_offset,
                                        density: self.state.print_density,
                                        alignment: self.state.alignment.clone(),
                                        bytes_per_line,
                                        print_area_width: self.state.print_area_width,
                                    });
                                    self.state.horizontal_offset = 0;
                                    self.log_debug(&format!(
                                        "FS p: rendered NV image slot {}",
                                        slot
                                    ));
                                } else {
                                    self.log_debug(&format!(
                                        "FS p: NV image slot {} not defined",
                                        slot
                                    ));
                                }
                            }
                        }
                        b'q' => {
                            // FS q n [xL xH yL yH d1...dk]... - Define NV bit image
                            if i < data.len() {
                                let n = data[i];
                                i += 1;
                                for slot in 1..=n {
                                    if i + 3 >= data.len() {
                                        break;
                                    }
                                    let xl = data[i] as usize;
                                    let xh = data[i + 1] as usize;
                                    let yl = data[i + 2] as usize;
                                    let yh = data[i + 3] as usize;
                                    let width_bytes = xl + (xh << 8);
                                    let height = yl + (yh << 8);
                                    let width = width_bytes * 8;
                                    let data_size = width_bytes * height;
                                    i += 4;

                                    if i + data_size <= data.len() && data_size > 0 {
                                        let img_data =
                                            data[i..i + data_size].to_vec();
                                        self.nv_images.lock().unwrap().insert(
                                            slot,
                                            NvBitImage {
                                                width,
                                                height,
                                                data: img_data,
                                            },
                                        );
                                        self.log_debug(&format!(
                                            "FS q: stored NV image slot {} ({}x{})",
                                            slot, width, height
                                        ));
                                    }
                                    i += data_size.min(data.len() - i);
                                }
                            }
                        }
                        b'(' => {
                            // FS ( fn pL pH [data...] - Extended commands with length
                            if i + 3 < data.len() {
                                let _fn = data[i]; // function code (e.g., 'A')
                                let p_l = data[i + 1] as usize;
                                let p_h = data[i + 2] as usize;
                                let len = p_l + (p_h << 8);
                                i += 3 + len.min(data.len() - i);
                            }
                        }
                        b'C' | b'g' | b'!' | b'&' | b'S' | b'-' => {
                            // Commands with 1 parameter
                            if i < data.len() {
                                i += 1;
                            }
                        }
                        _ => {
                            // Unknown FS subcommands - try to consume 1-2 likely parameter bytes
                            // Many proprietary commands use 1-2 bytes
                            if i < data.len() && (data[i] < 0x1B || data[i] > 0x7E) {
                                // Next byte doesn't look like a command start, consume it as parameter
                                i += 1;
                                // If it was high-bit, might be a 2-byte parameter
                                if i < data.len()
                                    && data[i - 1] > 0x7F
                                    && (data[i] < 0x1B || data[i] > 0x7E)
                                {
                                    i += 1;
                                }
                            }
                            if self.debug {
                                self.log_debug(&format!(
                                    "FS command 0x{:02X} - consumed {} parameter bytes",
                                    cmd,
                                    i - (start_pos + 2)
                                ));
                            }
                        }
                    }
                    // Command processed - allow text accumulation again
                    self.log_command(
                        &format!("1C {:02X}", cmd),
                        &format!("FS {}", cmd as char),
                    );
                    self.in_command_sequence = false;
                }
                LF => {
                    // LF: Print and line feed - flush current line and advance
                    self.in_command_sequence = false; // Exit command sequence, allow text again
                    self.last_was_binary = false; // LF marks start of text content
                    if !self.current_line.is_empty() {
                        self.flush_line();
                        self.current_line.clear();
                    } else if !self.elements.is_empty() {
                        // Only add separator for blank lines if we've already printed something
                        // This avoids extra spacing after init commands like ESC @
                        self.elements.push(ReceiptElement::Separator);
                    }
                    i += 1;
                }
                CR => {
                    // CR: Print and carriage return - flush current line
                    self.in_command_sequence = false; // Exit command sequence, allow text again
                    self.last_was_binary = false; // CR marks start of text content
                    if !self.current_line.is_empty() {
                        self.flush_line();
                        self.current_line.clear();
                    }
                    i += 1;
                }
                FF => {
                    self.current_line.clear();
                    // Only add FormFeed if the last element isn't already one
                    if !matches!(self.elements.last(), Some(ReceiptElement::FormFeed)) {
                        self.elements.push(ReceiptElement::FormFeed);
                    }
                    i += 1;
                }
                HT => {
                    // Only add tabs if not in command sequence
                    if !self.in_command_sequence {
                        // Add 4 spaces as tab
                        self.current_line.extend_from_slice(b"    ");
                    }
                    i += 1;
                }
                0x20..=0x7E | 0x80..=0xFF => {
                    // Printable characters (both ASCII and extended codepage)
                    if i == data.len() - 1 && !self.buffer.is_empty() {
                        break;
                    }
                    // Only accumulate text if we're NOT in a command sequence AND not after binary data
                    if !self.in_command_sequence && !self.last_was_binary {
                        if self.debug {
                            self.log_debug(&format!(
                                "Adding byte to line: 0x{:02X} at position {}",
                                byte, i
                            ));
                        }
                        self.current_line.push(byte);
                    }
                    i += 1;
                }
                0x00..=0x1F | 0x7F => {
                    // Control characters (including DEL)
                    // Silently consume these - they're control codes, not printable text
                    i += 1;
                }
            }
        }

        self.buffer.drain(0..i);

        // Don't auto-flush at buffer end - only flush on explicit line terminators (LF, CR)
        // This prevents fragmenting text that arrives in multiple TCP packets

        Ok(())
    }

    fn flush_line(&mut self) {
        if self.current_line.is_empty() {
            return;
        }

        if self.debug {
            self.log_debug(&format!(
                "Flushing line: {} bytes, codepage={}",
                self.current_line.len(),
                self.state.code_page
            ));
        }

        // Decode bytes using current codepage
        let decoded = if let Some(cp_num) = escpos_to_windows_cp(self.state.code_page) {
            // OEM/DOS codepage — use oem_cp tables (encoding_rs doesn't cover these)
            DECODING_TABLE_CP_MAP
                .get(&cp_num)
                .map(|t| t.decode_string_lossy(&self.current_line))
                .unwrap_or_else(|| String::from_utf8_lossy(&self.current_line).into_owned())
        } else {
            // Windows-125x, Shift-JIS, etc. — use encoding_rs
            let (decoded_cow, _encoding_used, had_errors) =
                self.state.encoding.decode(&self.current_line);

            if self.debug {
                if had_errors {
                    self.log_debug(&format!(
                        "Decoding errors in line, codepage={}",
                        self.state.code_page
                    ));
                }
                self.log_debug(&format!("Decoded: {:?}", decoded_cow));
            }

            decoded_cow.into_owned()
        };

        self.elements.push(ReceiptElement::Text {
            content: decoded,
            bold: self.state.bold,
            underline: self.state.underline,
            double_width: self.state.double_width,
            double_height: self.state.double_height,
            inverted: self.state.inverted,
            alignment: self.state.alignment.clone(),
            density: self.state.print_density,
            offset: self.state.horizontal_offset,
            left_margin: self.state.left_margin,
            character_spacing: self.state.character_spacing,
            double_strike: self.state.double_strike,
            font: self.state.font,
            print_area_width: self.state.print_area_width,
        });

        // Reset horizontal offset after use (ESC $ is one-time positioning)
        self.state.horizontal_offset = 0;
    }

    fn handle_esc_command(&mut self, data: &[u8], mut i: usize) -> Result<usize> {
        let cmd = data[i];
        match cmd {
            b'@' => {
                self.state = PrinterState::default();
                self.barcode_hri_position = HriPosition::None;
                self.barcode_height = 162;
                self.barcode_module_width = 3;
                self.pdf417_module_width = 3;
                self.pdf417_error_correction = 1;
                self.pdf417_columns = 0;
                self.pdf417_rows = 0;
                self.pdf417_truncated = false;
                self.datamatrix_module_size = 3;
                i += 1;
            }
            b'E' => {
                i += 1;
                if i < data.len() {
                    self.state.bold = data[i] == 1;
                    i += 1;
                }
            }
            b'-' => {
                i += 1;
                if i < data.len() {
                    let n = data[i];
                    // n = 0: off, n = 1 or 2: on (with thickness)
                    // Only consider actual values 1-2, not ASCII '1' '2'
                    self.state.underline = n == 1 || n == 2;
                    i += 1;
                }
            }
            b'a' => {
                i += 1;
                if i < data.len() {
                    self.state.alignment = match data[i] {
                        0 => Alignment::Left,
                        1 => Alignment::Center,
                        2 => Alignment::Right,
                        _ => Alignment::Left,
                    };
                    i += 1;
                }
            }
            b'!' => {
                i += 1;
                if i < data.len() {
                    let mode = data[i];
                    self.state.bold = (mode & 0x08) != 0;
                    self.state.double_height = (mode & 0x10) != 0;
                    self.state.double_width = (mode & 0x20) != 0;
                    self.state.underline = (mode & 0x80) != 0;
                    i += 1;
                }
            }
            b'd' => {
                // ESC d n - Print and feed n lines
                i += 1;
                if i < data.len() {
                    let lines = data[i] as usize;
                    self.log_debug(&format!("ESC d: print and feed {} lines", lines));
                    // "Print and feed" — flush buffered text first, then feed n lines.
                    // Each iteration mirrors the LF handler: flush text if present,
                    // otherwise push a blank Separator (if something has been printed).
                    for _ in 0..lines {
                        if !self.current_line.is_empty() {
                            self.flush_line();
                            self.current_line.clear();
                        } else if !self.elements.is_empty() {
                            self.elements.push(ReceiptElement::Separator);
                        }
                    }
                    i += 1;
                }
            }
            b'*' => {
                i += 1;
                i = self.handle_raster_graphics(data, i)?;
            }
            b'~' => {
                i += 1;
                if i < data.len() {
                    self.state.print_density = data[i].min(8);
                    i += 1;
                }
            }
            b'p' => {
                i += 1;
                if i + 2 < data.len() {
                    let pin = data[i];
                    let on_time = data[i + 1];
                    let off_time = data[i + 2];
                    self.printer_status.lock().unwrap().drawer_open = true;
                    self.elements.push(ReceiptElement::CashDrawer {
                        pin,
                        on_time,
                        off_time,
                    });
                    i += 3;
                }
            }
            b' ' => {
                // ESC SP n - Set right-side character spacing
                i += 1;
                if i < data.len() {
                    self.state.character_spacing = data[i];
                    self.log_debug(&format!("ESC SP: character spacing = {}", data[i]));
                    i += 1;
                }
            }
            b'$' => {
                // ESC $ - Set absolute horizontal print position
                i += 1;
                if i + 1 < data.len() {
                    let nl = data[i] as u16;
                    let nh = data[i + 1] as u16;
                    self.state.horizontal_offset = nl + (nh << 8);
                    self.log_debug(&format!(
                        "ESC $: set horizontal offset to {}",
                        self.state.horizontal_offset
                    ));
                    i += 2;
                }
            }
            b'\\' => {
                // ESC \ - Set relative horizontal print position
                i += 1;
                if i + 1 < data.len() {
                    let nl = data[i] as i16;
                    let nh = data[i + 1] as i16;
                    let relative_offset = nl + (nh << 8);
                    // Add to current horizontal offset (can be negative)
                    self.state.horizontal_offset =
                        ((self.state.horizontal_offset as i16) + relative_offset).max(0) as u16;
                    self.log_debug(&format!(
                        "ESC \\: relative offset {} -> total {}",
                        relative_offset, self.state.horizontal_offset
                    ));
                    i += 2;
                }
            }
            b'K' | b'L' | b'Y' | b'Z' => {
                // ESC K/L/Y/Z - Select bit image mode
                i += 1;
                if i + 1 < data.len() {
                    let nl = data[i] as usize;
                    let nh = data[i + 1] as usize;
                    let width = nl + (nh << 8);
                    i += 2;
                    // Skip image data
                    let bytes_needed = match cmd {
                        b'K' | b'L' => width,
                        b'Y' | b'Z' => width * 2,
                        _ => width,
                    };
                    if i + bytes_needed <= data.len() {
                        i += bytes_needed;
                    }
                }
            }
            b'D' => {
                // ESC D - Set horizontal tab positions
                i += 1;
                // Read tab positions until NUL
                while i < data.len() && data[i] != 0 {
                    i += 1;
                }
                if i < data.len() {
                    i += 1; // skip NUL
                }
            }
            b'S' | b'T' | b'U' | b'W' => {
                // ESC S/T - Standard/page mode selection
                // ESC U - Unidirectional printing
                // ESC W - Set print area in page mode
                i += 1;
                if i < data.len() {
                    if cmd == b'W' && i + 7 < data.len() {
                        // W takes 8 parameters
                        i += 8;
                    } else {
                        i += 1;
                    }
                }
            }
            b'c' => {
                // ESC c - Paper sensor commands
                i += 1;
                if i + 1 < data.len() {
                    i += 2;
                }
            }
            b'i' => {
                // ESC i - Partial cut (obsolete)
                i += 1;
            }
            b's' => {
                // ESC s - Select paper sensor(s)
                i += 1;
                if i < data.len() {
                    i += 1;
                }
            }
            0x06 => {
                // ESC ACK n - Enable/disable panel buttons (or ASB in some implementations)
                i += 1;
                if i < data.len() {
                    let _n = data[i];
                    self.log_debug(&format!(
                        "ESC ACK: n=0x{:02X} (acknowledged, not implemented)",
                        _n
                    ));
                    i += 1;
                }
            }
            b'u' => {
                // ESC u - Transmit peripheral device status (obsolete)
                i += 1;
                if i < data.len() {
                    i += 1;
                }
            }
            b'v' => {
                // ESC v - Transmit paper sensor status (obsolete)
                i += 1;
                if i < data.len() {
                    i += 1;
                }
            }
            b't' => {
                // ESC t - Select character code table (ESC/POS standard)
                i += 1;
                if i < data.len() {
                    self.state.code_page = data[i];
                    // OEM/DOS codepages (0,2-5,14,17-19) are decoded via oem_cp in
                    // flush_line(); the encoding field is only used for the remaining
                    // paths that go through encoding_rs (Win-1252, Shift-JIS, etc.).
                    self.state.encoding = match data[i] {
                        16              => encoding_rs::WINDOWS_1252, // Windows-1252
                        20 | 21 | 255   => encoding_rs::SHIFT_JIS,    // Shift JIS (Japanese)
                        _               => encoding_rs::WINDOWS_1252, // OEM pages or fallback
                    };
                    if self.debug {
                        self.log_debug(&format!("ESC t: selected codepage {}", data[i]));
                    }
                    i += 1;
                }
            }
            b'M' => {
                // ESC M n - Select character font
                // n=0: Font A, n=1: Font B, n=2: Font C (if supported)
                i += 1;
                if i < data.len() {
                    self.state.font = data[i];
                    self.log_debug(&format!("ESC M: font = {}", data[i]));
                    i += 1;
                }
            }
            b'R' | b'r' | b'%' => {
                // Character set, region, user-defined char mode
                i += 1;
                if i < data.len() {
                    i += 1;
                }
            }
            b'2' => {
                // ESC 2 - Set default line spacing (1/6 inch = ~30 dots at 203 DPI)
                self.state.line_spacing = 30;
                self.log_debug("ESC 2: reset to default line spacing (30 dots)");
                i += 1;
            }
            b'3' => {
                // ESC 3 n - Set line spacing to n dots
                i += 1;
                if i < data.len() {
                    self.state.line_spacing = data[i];
                    self.log_debug(&format!("ESC 3: line spacing = {} dots", data[i]));
                    i += 1;
                }
            }
            b'{' => {
                // Upside down mode
                i += 1;
                if i < data.len() {
                    i += 1;
                }
            }
            b'G' => {
                // ESC G n - Double-strike mode (makes text darker/bolder)
                i += 1;
                if i < data.len() {
                    self.state.double_strike = data[i] != 0;
                    self.log_debug(&format!(
                        "ESC G: double-strike = {}",
                        self.state.double_strike
                    ));
                    i += 1;
                }
            }
            b'J' => {
                // ESC J n - Print and feed paper n dots (used by zj-58 CUPS driver)
                i += 1;
                if i < data.len() {
                    let dots = data[i];
                    self.log_debug(&format!("ESC J: print and feed {} dots", dots));
                    // Flush buffered text first (the "print" part of "print and feed")
                    if !self.current_line.is_empty() {
                        self.flush_line();
                        self.current_line.clear();
                    }
                    // ESC J feeds paper by dots, not full lines.  For display purposes
                    // treat each ~24-dot increment as roughly one line separator so the
                    // receipt doesn't collapse, while still avoiding runaway whitespace
                    // for tiny advances.
                    let line_separators = (dots as usize).div_ceil(24).min(4);
                    for _ in 0..line_separators {
                        self.elements.push(ReceiptElement::Separator);
                    }
                    i += 1;
                }
            }
            b'V' => {
                // 90-degree rotation
                i += 1;
                if i < data.len() {
                    i += 1;
                }
            }
            b'(' => {
                // ESC ( - Extended commands
                i += 1;
                if i + 2 < data.len() {
                    let p_l = data[i + 1] as usize;
                    let p_h = data[i + 2] as usize;
                    let len = p_l + (p_h << 8);
                    i += 3 + len;
                }
            }
            b'&' => {
                // ESC & - Define user-defined characters
                i += 1;
                if i + 2 < data.len() {
                    let y = data[i] as usize;
                    let c1 = data[i + 1] as usize;
                    let c2 = data[i + 2] as usize;
                    i += 3;
                    let num_chars = if c2 >= c1 { c2 - c1 + 1 } else { 0 };
                    let bytes_per_char = y * 12_usize.div_ceil(8);
                    i += num_chars * bytes_per_char;
                }
            }
            b'?' => {
                // ESC ? - Cancel user-defined characters
                i += 1;
                if i < data.len() {
                    i += 1;
                }
            }
            b'=' => {
                // ESC = - Select peripheral device
                i += 1;
                if i < data.len() {
                    i += 1;
                }
            }
            b'<' => {
                // ESC < - Return home
                i += 1;
            }
            _ => {
                // Unknown ESC command - assume it has at least 1 parameter
                if self.debug {
                    self.log_debug(&format!("Unknown ESC command: 0x{:02X}", cmd));
                }
                i += 1;
                // Try to consume 1 parameter byte to prevent leakage
                if i < data.len() {
                    i += 1;
                }
            }
        }
        Ok(i)
    }

    fn handle_gs_command(&mut self, data: &[u8], mut i: usize) -> Result<usize> {
        let cmd = data[i];
        match cmd {
            b'8' => {
                // GS 8 - Extended command (L = raster graphics)
                let start_i = i - 1;
                i += 1;
                if i < data.len() {
                    if data[i] == b'L' {
                        i = self.handle_gs_8l(data, i)?;
                    } else {
                        // Other GS 8 subcommands (structure: GS 8 fn p1 p2 p3 p4 data...)
                        let subcmd = data[i];
                        i += 1; // skip subcommand

                        // Read length bytes
                        if i + 4 > data.len() {
                            // Not enough data for length - wait for more
                            if self.debug {
                                self.log_debug(&format!(
                                    "GS 8 0x{:02X}: waiting for length bytes",
                                    subcmd
                                ));
                            }
                            return Ok(start_i);
                        }

                        let p1 = data[i] as usize;
                        let p2 = data[i + 1] as usize;
                        let p3 = data[i + 2] as usize;
                        let p4 = data[i + 3] as usize;
                        let len = p1 | (p2 << 8) | (p3 << 16) | (p4 << 24);
                        i += 4;

                        // Check if we have all the data
                        let skip = len.min(1_000_000);
                        if i + skip > data.len() {
                            // Not enough data - wait for more
                            if self.debug {
                                self.log_debug(&format!(
                                    "GS 8 0x{:02X}: waiting for {} data bytes (have {})",
                                    subcmd,
                                    skip,
                                    data.len() - i
                                ));
                            }
                            return Ok(start_i);
                        }

                        // Skip all the data
                        i += skip;
                    }
                }
            }
            b'V' => {
                i += 1;
                if i < data.len() {
                    i = self.handle_paper_cut(data, i)?;
                }
            }
            b'v' => {
                i += 1;
                if i < data.len() {
                    i = self.handle_raster_graphics_gs(data, i)?;
                }
            }
            b'!' => {
                // GS ! - Select character size (width and height multipliers)
                // Bits 0-2: width (0-7), Bits 4-6: height (0-7)
                i += 1;
                if i < data.len() {
                    let mode = data[i];
                    let width_mul = (mode & 0x07) + 1;
                    let height_mul = ((mode >> 4) & 0x07) + 1;
                    self.state.double_width = width_mul > 1;
                    self.state.double_height = height_mul > 1;
                    i += 1;
                }
            }
            b'B' => {
                i += 1;
                if i < data.len() {
                    self.state.inverted = data[i] == 1;
                    i += 1;
                }
            }
            b'L' => {
                // GS L nL nH - Set left margin (in dots)
                i += 1;
                if i + 1 < data.len() {
                    let nl = data[i] as u16;
                    let nh = data[i + 1] as u16;
                    self.state.left_margin = nl + (nh << 8);
                    self.log_debug(&format!(
                        "GS L: left margin = {} dots",
                        self.state.left_margin
                    ));
                    i += 2;
                }
            }
            b'W' => {
                // GS W nL nH - Set print area width (in dots)
                i += 1;
                if i + 1 < data.len() {
                    let nl = data[i] as u16;
                    let nh = data[i + 1] as u16;
                    self.state.print_area_width = nl + (nh << 8);
                    self.log_debug(&format!(
                        "GS W: print area width = {} dots",
                        self.state.print_area_width
                    ));
                    i += 2;
                }
            }
            b'H' => {
                // GS H n - Set HRI (Human Readable Interpretation) position
                i += 1;
                if i < data.len() {
                    self.barcode_hri_position = match data[i] {
                        1 => HriPosition::Above,
                        2 => HriPosition::Below,
                        3 => HriPosition::Both,
                        _ => HriPosition::None,
                    };
                    self.log_debug(&format!(
                        "GS H: HRI position = {:?}",
                        self.barcode_hri_position
                    ));
                    i += 1;
                }
            }
            b'h' => {
                // GS h n - Set barcode height (1-255 dots, default 162)
                i += 1;
                if i < data.len() {
                    self.barcode_height = data[i].max(1);
                    self.log_debug(&format!("GS h: barcode height = {}", self.barcode_height));
                    i += 1;
                }
            }
            b'w' => {
                // GS w n - Set barcode module width (1-6, default 3)
                i += 1;
                if i < data.len() {
                    self.barcode_module_width = data[i].clamp(1, 6);
                    self.log_debug(&format!(
                        "GS w: barcode width = {}",
                        self.barcode_module_width
                    ));
                    i += 1;
                }
            }
            b'k' => {
                // GS k m [data] - Print barcode
                i += 1;
                if i < data.len() {
                    let barcode_type_byte = data[i];
                    i += 1;

                    let (barcode_type, barcode_data) = if barcode_type_byte < 6 {
                        // Format A: GS k m d1...dk NUL (NUL-terminated)
                        let bt = match barcode_type_byte {
                            0 => BarcodeType::UpcA,
                            1 => BarcodeType::UpcE,
                            2 => BarcodeType::Ean13,
                            3 => BarcodeType::Ean8,
                            4 => BarcodeType::Code39,
                            5 => BarcodeType::Itf,
                            _ => unreachable!(),
                        };
                        let start = i;
                        while i < data.len() && data[i] != 0 {
                            i += 1;
                        }
                        let d = data[start..i].to_vec();
                        if i < data.len() {
                            i += 1; // skip NUL
                        }
                        (bt, d)
                    } else if (65..=73).contains(&barcode_type_byte) {
                        // Format B: GS k m n d1...dn (length-prefixed)
                        let bt = match barcode_type_byte {
                            65 => BarcodeType::UpcA,
                            66 => BarcodeType::UpcE,
                            67 => BarcodeType::Ean13,
                            68 => BarcodeType::Ean8,
                            69 => BarcodeType::Code39,
                            70 => BarcodeType::Itf,
                            71 => BarcodeType::Codabar,
                            72 => BarcodeType::Code93,
                            73 => BarcodeType::Code128,
                            _ => unreachable!(),
                        };
                        if i < data.len() {
                            let len = data[i] as usize;
                            i += 1;
                            let end = (i + len).min(data.len());
                            let d = data[i..end].to_vec();
                            i = end;
                            (bt, d)
                        } else {
                            return Ok(i);
                        }
                    } else {
                        self.log_debug(&format!(
                            "GS k: unknown barcode type {}",
                            barcode_type_byte
                        ));
                        return Ok(i);
                    };

                    if !barcode_data.is_empty() {
                        if !self.current_line.is_empty() {
                            self.flush_line();
                            self.current_line.clear();
                        }

                        self.elements.push(ReceiptElement::Barcode {
                            data: barcode_data,
                            barcode_type,
                            height: self.barcode_height,
                            module_width: self.barcode_module_width,
                            hri_position: self.barcode_hri_position,
                            alignment: self.state.alignment.clone(),
                            offset: self.state.horizontal_offset,
                            print_area_width: self.state.print_area_width,
                        });

                        self.state.horizontal_offset = 0;
                    }
                }
            }
            b'(' => {
                // Extended commands (QR Code, PDF417, DataMatrix)
                i += 1;
                if i < data.len() {
                    let subcmd = data[i];
                    if subcmd == b'k' {
                        i = self.handle_gs_paren_k(data, i)?;
                    } else {
                        // Other extended commands
                        if i + 2 < data.len() {
                            let p_l = data[i + 1] as usize;
                            let p_h = data[i + 2] as usize;
                            let len = p_l + (p_h << 8);
                            i += 3 + len;
                        }
                    }
                }
            }
            b'a' => {
                // GS a n - Enable/disable Automatic Status Back (ASB)
                i += 1;
                if i < data.len() {
                    let asb_flags = data[i];
                    self.log_debug(&format!("GS a: ASB flags=0x{:02X}", asb_flags));

                    if asb_flags != 0 {
                        let status = self.printer_status.lock().unwrap();
                        // Byte 0: bit 2=drawer, bit 3=offline, bit 4=1(fixed), bit 5=cover
                        let mut b0: u8 = 0x10; // bit 4 fixed
                        if status.drawer_open {
                            b0 |= 0x04; // bit 2
                        }
                        if !status.online {
                            b0 |= 0x08; // bit 3: offline
                        }
                        if !status.cover_closed {
                            b0 |= 0x20; // bit 5: cover open
                        }
                        // Byte 2: bit 0-1 = paper near-end, bit 2-3 = paper out
                        let mut b2: u8 = 0x00;
                        if !status.paper_present {
                            b2 |= 0x0C; // bits 2-3: paper out
                        }
                        drop(status);

                        self.response_queue.push(b0);
                        self.response_queue.push(0x00); // byte 1: no errors
                        self.response_queue.push(b2);
                        self.response_queue.push(0x00); // byte 3: reserved
                        self.log_debug(&format!(
                            "GS a: queued ASB [{:02X} 00 {:02X} 00]",
                            b0, b2
                        ));
                    }
                    i += 1;
                }
            }
            b'I' => {
                // GS I n - Transmit printer ID information
                // Response format: 0x5f + "string" + 0x00 (block data format)
                i += 1;
                if i < data.len() {
                    let n = data[i];
                    self.log_debug(&format!("GS I: query type=0x{:02X}", n));

                    // Queue response based on query type (block data format)
                    match n {
                        0x42 => {
                            // Manufacturer name (0x42 = 66)
                            // Send in block data format: 0x5f + "CITIZEN" + 0x00
                            // (use CITIZEN not EPSON so receiptio switches to 'escpos' mode)
                            self.response_queue.push(0x5f); // Block data start
                            self.response_queue.extend_from_slice(b"CITIZEN");
                            self.response_queue.push(0x00); // Null terminator
                            self.log_debug("GS I 0x42: sent manufacturer 'CITIZEN' (block data)");
                        }
                        0x43 => {
                            // Model name (0x43 = 67)
                            // Send in block data format: 0x5f + "CT-S310" + 0x00
                            self.response_queue.push(0x5f); // Block data start
                            self.response_queue.extend_from_slice(b"CT-S310");
                            self.response_queue.push(0x00); // Null terminator
                            self.log_debug("GS I 0x43: sent model 'CT-S310' (block data)");
                        }
                        _ => {
                            self.log_debug(&format!("GS I: unknown query type 0x{:02X}", n));
                        }
                    }
                    i += 1;
                }
            }
            b'r' => {
                // GS r n - Transmit status
                i += 1;
                if i < data.len() {
                    let _n = data[i];
                    self.log_debug(&format!("GS r: transmit status n=0x{:02X}", _n));

                    let status = self.printer_status.lock().unwrap();
                    let mut byte: u8 = 0x00;
                    if status.paper_present {
                        byte |= 0x08; // bit 3: paper present
                    }
                    if !status.online {
                        byte |= 0x10; // bit 4: offline
                    }
                    drop(status);
                    self.response_queue.push(byte);
                    self.log_debug(&format!("GS r: queued status 0x{:02X}", byte));
                    i += 1;
                }
            }
            b'$' => {
                // GS $ nL nH - Set absolute vertical print position
                // Used by receiptio for positioning each line
                i += 1;
                if i + 1 < data.len() {
                    let nl = data[i] as u16;
                    let nh = data[i + 1] as u16;
                    let vertical_pos = nl + (nh << 8);
                    self.log_debug(&format!("GS $: set vertical position to {}", vertical_pos));
                    // VirtualESC renders sequentially, so we acknowledge but don't use this
                    i += 2;
                }
            }
            0x00 | 0x80 | 0xF7 => {
                // Additional GS commands found in real data
                i += 1;
                // Consume likely parameter
                if i < data.len() {
                    i += 1;
                }
            }
            _ => {
                // Unknown GS command - assume it has at least 1 parameter
                if self.debug {
                    self.log_debug(&format!("Unknown GS command: 0x{:02X}", cmd));
                }
                i += 1;
                // Try to consume 1 parameter byte to prevent leakage
                if i < data.len() {
                    i += 1;
                }
            }
        }
        Ok(i)
    }

    fn handle_raster_graphics(&mut self, data: &[u8], i: usize) -> Result<usize> {
        let start_i = i - 2; // Point to ESC byte, not '*' byte (i-1=*, i-2=ESC)

        if i + 3 > data.len() {
            self.log_debug("ESC * incomplete: not enough header bytes");
            return Ok(start_i);
        }

        let m = data[i];
        let nl = data[i + 1] as usize;
        let nh = data[i + 2] as usize;
        let width = nl + (nh << 8);
        let height = match m {
            0 | 1 => 8,
            32 | 33 => 24,
            _ => 8,
        };

        let mut pos = i + 3;

        // Validate dimensions
        if width == 0 || width > 10000 {
            self.log_debug(&format!("ESC * invalid width: {}", width));
            return Ok(pos);
        }

        // ESC * uses COLUMN-based format, not raster!
        // Each column is height/8 bytes (8-dot) or height/8*3 bytes (24-dot)
        let bytes_per_column = height / 8;
        let total_bytes = width * bytes_per_column;

        self.log_debug(&format!(
            "ESC * column-based: m={}, width={}, height={}, bytes_per_col={}, need {} bytes",
            m, width, height, bytes_per_column, total_bytes
        ));

        if total_bytes > 1_000_000 {
            self.log_debug("ESC * dimensions too large, skipping");
            return Ok(pos);
        }

        if pos + total_bytes > data.len() {
            self.log_debug(&format!(
                "ESC * incomplete: have {}, need {}",
                data.len() - pos,
                total_bytes
            ));
            return Ok(start_i);
        }

        // Additional safety check before slicing
        if pos >= data.len() || pos + total_bytes > data.len() {
            self.log_debug("ESC * bounds check failed");
            return Ok(start_i);
        }

        // Flush any pending text before image
        if !self.current_line.is_empty() {
            self.flush_line();
            self.current_line.clear();
        }

        // Convert column-based data to row-based raster data for rendering
        let column_data = &data[pos..pos + total_bytes];
        let raster_data = self.column_to_raster(column_data, width, height);

        self.elements.push(ReceiptElement::RasterImage {
            width,
            height,
            data: raster_data,
            offset: self.state.horizontal_offset,
            density: self.state.print_density,
            alignment: self.state.alignment.clone(),
            bytes_per_line: width.div_ceil(8), // Calculate from pixel width
            print_area_width: self.state.print_area_width,
        });

        // Reset offset after rendering
        self.state.horizontal_offset = 0;

        // Mark that we just processed binary data - don't treat following ASCII bytes as text
        self.last_was_binary = true;

        pos += total_bytes;

        Ok(pos)
    }

    fn column_to_raster(&self, column_data: &[u8], width: usize, height: usize) -> Vec<u8> {
        let bytes_per_column = height / 8;
        let bytes_per_row = width.div_ceil(8);
        let mut raster_data = vec![0u8; bytes_per_row * height];

        // Convert column format to raster format
        // Column format: each byte represents 8 vertical pixels in a column
        // Raster format: each byte represents 8 horizontal pixels in a row

        for col in 0..width {
            let column_offset = col * bytes_per_column;

            for byte_in_col in 0..bytes_per_column {
                if column_offset + byte_in_col >= column_data.len() {
                    break;
                }

                let col_byte = column_data[column_offset + byte_in_col];

                // Each bit in this byte represents a pixel at a different row
                for bit in 0..8 {
                    let y = byte_in_col * 8 + bit;
                    if y >= height {
                        break;
                    }

                    // Extract the pixel value (1 = black, 0 = white)
                    let pixel = (col_byte >> (7 - bit)) & 1;

                    // Set the corresponding bit in the raster data
                    let row_byte_idx = y * bytes_per_row + (col / 8);
                    let row_bit_idx = 7 - (col % 8);

                    if row_byte_idx < raster_data.len() {
                        raster_data[row_byte_idx] |= pixel << row_bit_idx;
                    }
                }
            }
        }

        raster_data
    }

    fn handle_raster_graphics_gs(&mut self, data: &[u8], i: usize) -> Result<usize> {
        let start_i = i - 2; // Point to GS byte, not 'v' byte (i-1=v, i-2=GS)

        self.log_debug(&format!("GS v: entered handler at position {}", i));

        if i + 6 > data.len() {
            self.log_debug(&format!(
                "GS v incomplete: not enough header bytes (have {}, need {})",
                data.len() - i,
                6
            ));
            return Ok(start_i);
        }

        // zj-58 format: GS v variant m xL xH yL yH [data]
        // escRasterMode[] = "\x1dv0\0" sends: GS v '0' 0x00
        // Then mputnum(width) and mputnum(height) send little-endian 2-byte values
        let variant = data[i]; // '0' = 0x30
        let _m = data[i + 1]; // 0x00 (mode)
        let xl = data[i + 2] as usize;
        let xh = data[i + 3] as usize;
        let yl = data[i + 4] as usize;
        let yh = data[i + 5] as usize;

        self.log_debug(&format!(
            "GS v: raw bytes at i: [{:02X} {:02X} {:02X} {:02X} {:02X} {:02X}]",
            data[i],
            data[i + 1],
            data[i + 2],
            data[i + 3],
            data[i + 4],
            data[i + 5]
        ));
        self.log_debug(&format!(
            "GS v: variant=0x{:02X} m=0x{:02X}, xl=0x{:02X} xh=0x{:02X} yl=0x{:02X} yh=0x{:02X}",
            variant, _m, xl, xh, yl, yh
        ));

        let mut pos = i + 6;

        // GS v 0: xL/xH are width in BYTES, yL/yH are height in DOTS (pixels)
        let width_in_bytes = xl + (xh << 8);
        let height = yl + (yh << 8);
        let width = width_in_bytes * 8; // Convert bytes to pixels for rendering

        // Validate dimensions
        if width_in_bytes == 0 || height == 0 {
            self.log_debug(&format!(
                "GS v invalid dimensions: {} bytes x {} pixels",
                width_in_bytes, height
            ));
            return Ok(pos);
        }

        if width > 10000 || height > 10000 {
            self.log_debug(&format!(
                "GS v dimensions too large: {}x{} pixels, attempting to skip raster data",
                width, height
            ));
            // Still need to skip the raster data even if dimensions seem wrong
            // Otherwise the raster bytes will be processed as text
            let total_bytes = width_in_bytes * height;
            if total_bytes > 5_000_000 {
                self.log_debug("GS v: calculated bytes too large, cannot skip safely");
                return Ok(start_i); // Wait for correct data or give up
            }
            if pos + total_bytes > data.len() {
                self.log_debug(&format!(
                    "GS v: not enough data to skip (need {} more bytes)",
                    total_bytes - (data.len() - pos)
                ));
                return Ok(start_i); // Wait for more data
            }
            return Ok(pos + total_bytes); // Skip past the raster data
        }

        let total_bytes = width_in_bytes * height;

        self.log_debug(&format!(
            "GS v raster: width={} pixels ({} bytes), height={} pixels, need {} bytes",
            width, width_in_bytes, height, total_bytes
        ));

        if total_bytes > 5_000_000 {
            self.log_debug("GS v raster: calculated bytes too large, skipping");
            return Ok(pos);
        }

        if pos + total_bytes > data.len() {
            self.log_debug(&format!(
                "GS v incomplete: have {}, need {}",
                data.len() - pos,
                total_bytes
            ));
            return Ok(start_i);
        }

        // Additional safety check before slicing
        if pos >= data.len() || pos + total_bytes > data.len() {
            self.log_debug("GS v bounds check failed");
            return Ok(start_i);
        }

        // Flush any pending text before image (already cleared by caller)
        if !self.current_line.is_empty() {
            self.flush_line();
            self.current_line.clear();
        }

        // Debug: dump first 64 bytes of raster data to see the pattern
        if self.debug {
            let preview_len = std::cmp::min(64, total_bytes);
            let mut hex_str = String::new();
            for i in 0..preview_len {
                hex_str.push_str(&format!("{:02X} ", data[pos + i]));
                if (i + 1) % 16 == 0 {
                    hex_str.push('\n');
                }
            }
            self.log_debug(&format!(
                "GS v raster data (first {} bytes):\n{}",
                preview_len, hex_str
            ));

            // Also show bytes per line calculation
            self.log_debug(&format!(
                "Width={} pixels -> {} bytes per line, {} total lines",
                width, width_in_bytes, height
            ));

            // Save raster data to a PBM file for inspection
            use std::io::Write;
            let filename = format!("raster_{}x{}.pbm", width, height);
            if let Ok(mut file) = std::fs::File::create(&filename) {
                // PBM format: P4 (binary)
                writeln!(file, "P4").ok();
                writeln!(file, "{} {}", width, height).ok();
                file.write_all(&data[pos..pos + total_bytes]).ok();
                self.log_debug(&format!("Saved raster to {}", filename));
            }
        }

        // GS v data is in standard raster format (row-based), NOT column format
        // Just use the data directly
        self.elements.push(ReceiptElement::RasterImage {
            width,
            height,
            data: data[pos..pos + total_bytes].to_vec(),
            offset: self.state.horizontal_offset,
            density: self.state.print_density,
            alignment: self.state.alignment.clone(),
            bytes_per_line: width_in_bytes, // Use actual bytes from command
            print_area_width: self.state.print_area_width,
        });

        // Reset offset after rendering
        self.state.horizontal_offset = 0;

        // Mark that we just processed binary data - don't treat following ASCII bytes as text
        self.last_was_binary = true;

        pos += total_bytes;

        Ok(pos)
    }

    fn handle_gs_8l(&mut self, data: &[u8], mut i: usize) -> Result<usize> {
        let start_i = i - 1;

        // GS 8 L p1 p2 p3 p4 m fn a bx by c xL xH yL yH d1...dk
        if i + 10 > data.len() {
            self.log_debug("GS 8 L incomplete: not enough header bytes");
            return Ok(start_i);
        }

        i += 1; // skip 'L'

        let p1 = data[i] as u32;
        let p2 = data[i + 1] as u32;
        let p3 = data[i + 2] as u32;
        let p4 = data[i + 3] as u32;
        let data_len = p1 | (p2 << 8) | (p3 << 16) | (p4 << 24);

        let m = data[i + 4];
        let _fn = data[i + 5];
        let _a = data[i + 6];
        let _bx = data[i + 7];
        let _by = data[i + 8];
        let _c = data[i + 9];

        i += 10;

        if m == 48 || m == 112 {
            if i + 4 > data.len() {
                self.log_debug("GS 8 L incomplete: not enough dimension bytes");
                return Ok(start_i);
            }

            let xl = data[i] as usize;
            let xh = data[i + 1] as usize;
            let yl = data[i + 2] as usize;
            let yh = data[i + 3] as usize;

            let width = xl | (xh << 8);
            let height = yl | (yh << 8);

            i += 4;

            let image_bytes = width.div_ceil(8) * height;

            self.log_debug(&format!(
                "GS 8 L raster: m={}, width={}, height={}, need {} bytes",
                m, width, height, image_bytes
            ));

            if data_len as usize > 100_000 || image_bytes > 5_000_000 {
                self.log_debug("GS 8 L: dimensions too large, skipping");
                // data_len includes m,fn,a,bx,by,c (6 bytes) which we already consumed
                // We need to skip the remaining data_len - 6 bytes
                let skip = (data_len as usize).saturating_sub(6);
                if i + skip <= data.len() {
                    return Ok(i + skip);
                } else {
                    // Not enough data to skip - wait for more
                    return Ok(start_i);
                }
            }

            if i + image_bytes > data.len() {
                self.log_debug(&format!(
                    "GS 8 L incomplete: have {}, need {}",
                    data.len() - i,
                    image_bytes
                ));
                return Ok(start_i);
            }

            if !self.current_line.is_empty() {
                self.flush_line();
                self.current_line.clear();
            }

            self.elements.push(ReceiptElement::RasterImage {
                width,
                height,
                data: data[i..i + image_bytes].to_vec(),
                offset: self.state.horizontal_offset,
                density: self.state.print_density,
                alignment: self.state.alignment.clone(),
                bytes_per_line: width.div_ceil(8), // Calculate from pixel width
                print_area_width: self.state.print_area_width,
            });

            // Reset offset after rendering
            self.state.horizontal_offset = 0;

            // Mark that we just processed binary data
            self.last_was_binary = true;

            i += image_bytes;
        } else {
            let skip = (data_len as usize).saturating_sub(6);
            i += skip.min(data.len() - i);
        }

        Ok(i)
    }

    fn handle_gs_paren_k(&mut self, data: &[u8], mut i: usize) -> Result<usize> {
        let start_i = i - 1;

        // GS ( k pL pH cn fn [parameters]
        if i + 4 > data.len() {
            self.log_debug("GS ( k incomplete: not enough header bytes");
            return Ok(start_i);
        }

        i += 1; // skip 'k'

        let p_l = data[i] as usize;
        let p_h = data[i + 1] as usize;
        let param_len = p_l | (p_h << 8);

        let cn = data[i + 2];
        let fn_code = data[i + 3];

        i += 4;

        match cn {
            48 => {
                // PDF417 commands
                match fn_code {
                    65 => {
                        if i < data.len() {
                            self.pdf417_columns = data[i];
                            self.log_debug(&format!(
                                "GS ( k PDF417: columns = {}",
                                self.pdf417_columns
                            ));
                            i += 1;
                        }
                    }
                    66 => {
                        if i < data.len() {
                            self.pdf417_rows = data[i];
                            self.log_debug(&format!(
                                "GS ( k PDF417: rows = {}",
                                self.pdf417_rows
                            ));
                            i += 1;
                        }
                    }
                    67 => {
                        if i < data.len() {
                            self.pdf417_module_width = data[i].clamp(1, 8);
                            self.log_debug(&format!(
                                "GS ( k PDF417: module width = {}",
                                self.pdf417_module_width
                            ));
                            i += 1;
                        }
                    }
                    68 => {
                        // Row height (consume)
                        if i < data.len() {
                            i += 1;
                        }
                    }
                    69 => {
                        if i < data.len() {
                            self.pdf417_error_correction = data[i];
                            self.log_debug(&format!(
                                "GS ( k PDF417: error correction = {}",
                                self.pdf417_error_correction
                            ));
                            i += 1;
                        }
                    }
                    70 => {
                        if i < data.len() {
                            self.pdf417_truncated = data[i] == 1;
                            i += 1;
                        }
                    }
                    80 => {
                        // Store PDF417 data: pL pH cn fn m d1...dk, skip m
                        if i >= data.len() {
                            self.log_debug("GS ( k PDF417 data incomplete (no m byte)");
                            return Ok(start_i);
                        }
                        i += 1; // skip m byte
                        let data_len = param_len.saturating_sub(3);
                        if i + data_len > data.len() {
                            self.log_debug("GS ( k PDF417 data incomplete");
                            return Ok(start_i);
                        }
                        self.pdf417_data = data[i..i + data_len].to_vec();
                        self.log_debug(&format!(
                            "GS ( k PDF417: stored {} bytes",
                            data_len
                        ));
                        i += data_len;
                    }
                    81 => {
                        // Transmit size (skip)
                        let skip = param_len.saturating_sub(2);
                        i += skip.min(data.len() - i);
                    }
                    82 => {
                        // Print PDF417: pL pH cn fn m, skip m
                        let skip = param_len.saturating_sub(2);
                        i += skip.min(data.len() - i);
                        if !self.pdf417_data.is_empty() {
                            if !self.current_line.is_empty() {
                                self.flush_line();
                                self.current_line.clear();
                            }

                            self.elements.push(ReceiptElement::Barcode2D {
                                data: self.pdf417_data.clone(),
                                variant: Barcode2DVariant::Pdf417,
                                module_size: self.pdf417_module_width,
                                alignment: self.state.alignment.clone(),
                                offset: self.state.horizontal_offset,
                                print_area_width: self.state.print_area_width,
                            });

                            self.state.horizontal_offset = 0;
                            self.pdf417_data.clear();
                            self.log_debug("GS ( k PDF417: print");
                        }
                    }
                    _ => {
                        let skip = param_len.saturating_sub(2);
                        i += skip.min(data.len() - i);
                    }
                }
            }
            49 => {
                // QR Code commands (existing logic)
                match fn_code {
                    65 => {
                        // Set QR model: params are n1 n2 (2 bytes)
                        let skip = (param_len.saturating_sub(2)).min(data.len() - i);
                        i += skip;
                    }
                    67 => {
                        // Set module size: param is n (1 byte)
                        if i < data.len() {
                            self.qr_size = data[i];
                            i += 1;
                        }
                    }
                    69 => {
                        // Set error correction level
                        if i < data.len() {
                            self.qr_error_correction = data[i];
                            i += 1;
                        }
                    }
                    80 => {
                        // Store QR data: pL pH cn fn m d1...dk
                        // param_len includes cn+fn+m+data, i is past cn+fn, skip m
                        if i >= data.len() {
                            self.log_debug("GS ( k QR data incomplete (no m byte)");
                            return Ok(start_i);
                        }
                        i += 1; // skip m (encoding mode byte)
                        let data_len = param_len.saturating_sub(3);
                        if i + data_len > data.len() {
                            self.log_debug("GS ( k QR data incomplete");
                            return Ok(start_i);
                        }
                        self.qr_data = data[i..i + data_len].to_vec();
                        i += data_len;
                    }
                    81 => {
                        // Print QR code: pL pH cn fn m, skip m
                        let skip = param_len.saturating_sub(2);
                        i += skip.min(data.len() - i);
                        if !self.qr_data.is_empty() {
                            if !self.current_line.is_empty() {
                                self.flush_line();
                                self.current_line.clear();
                            }

                            let qr_string =
                                String::from_utf8_lossy(&self.qr_data).to_string();
                            let size = (self.qr_size as usize).clamp(1, 16);

                            self.elements.push(ReceiptElement::QrCode {
                                data: qr_string,
                                size,
                                alignment: self.state.alignment.clone(),
                                offset: self.state.horizontal_offset,
                                print_area_width: self.state.print_area_width,
                            });

                            self.state.horizontal_offset = 0;
                            self.qr_data.clear();
                        }
                    }
                    _ => {
                        let skip = param_len.saturating_sub(2);
                        i += skip.min(data.len() - i);
                    }
                }
            }
            50 => {
                // DataMatrix commands
                match fn_code {
                    65 => {
                        // Symbol type (consume)
                        if i < data.len() {
                            i += 1;
                        }
                    }
                    67 => {
                        if i < data.len() {
                            self.datamatrix_module_size = data[i].clamp(1, 8);
                            self.log_debug(&format!(
                                "GS ( k DataMatrix: module size = {}",
                                self.datamatrix_module_size
                            ));
                            i += 1;
                        }
                    }
                    80 => {
                        // Store DataMatrix data: pL pH cn fn m d1...dk, skip m
                        if i >= data.len() {
                            self.log_debug("GS ( k DataMatrix data incomplete (no m byte)");
                            return Ok(start_i);
                        }
                        i += 1; // skip m byte
                        let data_len = param_len.saturating_sub(3);
                        if i + data_len > data.len() {
                            self.log_debug("GS ( k DataMatrix data incomplete");
                            return Ok(start_i);
                        }
                        self.datamatrix_data = data[i..i + data_len].to_vec();
                        self.log_debug(&format!(
                            "GS ( k DataMatrix: stored {} bytes",
                            data_len
                        ));
                        i += data_len;
                    }
                    81 => {
                        // Transmit size (skip)
                        let skip = param_len.saturating_sub(2);
                        i += skip.min(data.len() - i);
                    }
                    82 => {
                        // Print DataMatrix: pL pH cn fn m, skip m
                        let skip = param_len.saturating_sub(2);
                        i += skip.min(data.len() - i);
                        if !self.datamatrix_data.is_empty() {
                            if !self.current_line.is_empty() {
                                self.flush_line();
                                self.current_line.clear();
                            }

                            self.elements.push(ReceiptElement::Barcode2D {
                                data: self.datamatrix_data.clone(),
                                variant: Barcode2DVariant::DataMatrix,
                                module_size: self.datamatrix_module_size,
                                alignment: self.state.alignment.clone(),
                                offset: self.state.horizontal_offset,
                                print_area_width: self.state.print_area_width,
                            });

                            self.state.horizontal_offset = 0;
                            self.datamatrix_data.clear();
                            self.log_debug("GS ( k DataMatrix: print");
                        }
                    }
                    _ => {
                        let skip = param_len.saturating_sub(2);
                        i += skip.min(data.len() - i);
                    }
                }
            }
            _ => {
                let skip = param_len.saturating_sub(2);
                i += skip.min(data.len() - i);
            }
        }

        Ok(i)
    }

    fn handle_paper_cut(&mut self, data: &[u8], mut i: usize) -> Result<usize> {
        let mode = data[i];
        i += 1;

        let cut_type = match mode {
            0 | 48 => "FULL CUT",
            1 | 49 => "PARTIAL CUT",
            65 => "FEED & FULL CUT",
            66 => "FEED & PARTIAL CUT",
            _ => "UNKNOWN CUT",
        };

        self.flush_line();
        self.elements.push(ReceiptElement::PaperCut {
            cut_type: cut_type.to_string(),
        });

        Ok(i)
    }
}

struct PrinterStatus {
    paper_present: bool,
    cover_closed: bool,
    drawer_open: bool,
    online: bool,
}

impl Default for PrinterStatus {
    fn default() -> Self {
        Self {
            paper_present: true,
            cover_closed: true,
            drawer_open: false,
            online: true,
        }
    }
}

#[derive(Clone)]
struct NvBitImage {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct CommandLogEntry {
    hex: String,
    description: String,
}

#[derive(Clone)]
struct AppState {
    elements: Arc<Mutex<Vec<ReceiptElement>>>,
    connections: Arc<Mutex<Vec<String>>>,
    paper_size: Arc<Mutex<PaperSize>>,
    printer_status: Arc<Mutex<PrinterStatus>>,
    nv_images: Arc<Mutex<HashMap<u8, NvBitImage>>>,
    command_log: Arc<Mutex<Vec<CommandLogEntry>>>,
    port: u16,
}

impl AppState {
    fn new(port: u16) -> Self {
        Self {
            elements: Arc::new(Mutex::new(Vec::new())),
            connections: Arc::new(Mutex::new(Vec::new())),
            paper_size: Arc::new(Mutex::new(PaperSize::Size80mm)),
            printer_status: Arc::new(Mutex::new(PrinterStatus::default())),
            nv_images: Arc::new(Mutex::new(HashMap::new())),
            command_log: Arc::new(Mutex::new(Vec::new())),
            port,
        }
    }
}

struct VirtualEscPosApp {
    state: AppState,
    inspector_open: bool,
    export_status: Option<String>,
}

impl VirtualEscPosApp {
    fn new(_cc: &eframe::CreationContext, state: AppState) -> Self {
        Self {
            state,
            inspector_open: false,
            export_status: None,
        }
    }
}

impl eframe::App for VirtualEscPosApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint();

        // Handle screenshot events
        ctx.input(|i| {
            for event in &i.raw.events {
                if let egui::Event::Screenshot { image, .. } = event {
                    let width = image.width();
                    let height = image.height();
                    let pixels: Vec<u8> = image
                        .pixels
                        .iter()
                        .flat_map(|c| [c.r(), c.g(), c.b(), c.a()])
                        .collect();
                    if let Some(img_buf) =
                        image::RgbaImage::from_raw(width as u32, height as u32, pixels)
                    {
                        let path = "escpresso_receipt.png";
                        match img_buf.save(path) {
                            Ok(()) => {
                                self.export_status =
                                    Some(format!("Saved: {}", path));
                            }
                            Err(e) => {
                                self.export_status =
                                    Some(format!("Error: {}", e));
                            }
                        }
                    }
                }
            }
        });

        // Force light mode, ignoring OS dark mode
        ctx.set_visuals(egui::Visuals::light());

        let mut style = (*ctx.style()).clone();
        style.visuals.panel_fill = egui::Color32::WHITE;
        style.visuals.window_fill = egui::Color32::WHITE;
        style.visuals.popup_shadow = egui::epaint::Shadow::NONE;
        style.visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::BLACK;
        style.visuals.widgets.noninteractive.bg_fill = egui::Color32::WHITE;
        style.visuals.widgets.inactive.fg_stroke.color = egui::Color32::BLACK;
        style.visuals.widgets.inactive.bg_fill = egui::Color32::from_gray(245);
        style.visuals.widgets.active.fg_stroke.color = egui::Color32::BLACK;
        style.visuals.widgets.active.bg_fill = egui::Color32::from_gray(230);
        style.visuals.widgets.hovered.fg_stroke.color = egui::Color32::BLACK;
        style.visuals.widgets.hovered.bg_fill = egui::Color32::from_gray(250);
        style.visuals.widgets.open.fg_stroke.color = egui::Color32::BLACK;
        style.visuals.widgets.open.bg_fill = egui::Color32::from_gray(250);
        style.visuals.extreme_bg_color = egui::Color32::WHITE;
        style.visuals.faint_bg_color = egui::Color32::from_gray(250);
        style.visuals.selection.bg_fill = egui::Color32::from_gray(248);
        style.visuals.selection.stroke.color = egui::Color32::BLACK;
        ctx.set_style(style);

        let mut current_paper_size = *self.state.paper_size.lock().unwrap();
        let mut paper_size_changed = false;

        egui::TopBottomPanel::top("menu_bar")
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::WHITE)
                    .inner_margin(egui::Margin::symmetric(8.0, 5.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;

                    let widget_height = 22.0;
                    let rounding = 4.0;

                    // Shared style for neutral widgets (combo box, clear button)
                    let neutral_bg = egui::Color32::from_gray(240);
                    let neutral_hover = egui::Color32::from_gray(225);
                    let neutral_active = egui::Color32::from_gray(210);

                    ui.scope(|ui| {
                        let style = ui.style_mut();
                        let r = egui::Rounding::same(rounding);
                        for w in [
                            &mut style.visuals.widgets.inactive,
                            &mut style.visuals.widgets.noninteractive,
                            &mut style.visuals.widgets.open,
                        ] {
                            w.weak_bg_fill = neutral_bg;
                            w.bg_fill = neutral_bg;
                            w.fg_stroke.color = egui::Color32::BLACK;
                            w.rounding = r;
                        }
                        style.visuals.widgets.hovered.weak_bg_fill = neutral_hover;
                        style.visuals.widgets.hovered.bg_fill = neutral_hover;
                        style.visuals.widgets.hovered.fg_stroke.color = egui::Color32::BLACK;
                        style.visuals.widgets.hovered.rounding = r;
                        style.visuals.widgets.active.weak_bg_fill = neutral_active;
                        style.visuals.widgets.active.bg_fill = neutral_active;
                        style.visuals.widgets.active.fg_stroke.color = egui::Color32::BLACK;
                        style.visuals.widgets.active.rounding = r;
                        style.visuals.selection.bg_fill = egui::Color32::from_gray(232);
                        style.visuals.selection.stroke.color = egui::Color32::BLACK;

                        egui::ComboBox::from_id_salt("paper_size")
                            .selected_text(current_paper_size.label())
                            .height(widget_height)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_value(
                                        &mut current_paper_size,
                                        PaperSize::Size58mm,
                                        "58mm",
                                    )
                                    .clicked()
                                {
                                    let old_size = *self.state.paper_size.lock().unwrap();
                                    if old_size != PaperSize::Size58mm {
                                        *self.state.paper_size.lock().unwrap() =
                                            PaperSize::Size58mm;
                                        paper_size_changed = true;
                                    }
                                }
                                if ui
                                    .selectable_value(
                                        &mut current_paper_size,
                                        PaperSize::Size80mm,
                                        "80mm",
                                    )
                                    .clicked()
                                {
                                    let old_size = *self.state.paper_size.lock().unwrap();
                                    if old_size != PaperSize::Size80mm {
                                        *self.state.paper_size.lock().unwrap() =
                                            PaperSize::Size80mm;
                                        paper_size_changed = true;
                                    }
                                }
                            });

                        let clear_btn = egui::Button::new("Clear")
                            .min_size(egui::vec2(0.0, widget_height));
                        if ui.add(clear_btn).clicked() {
                            self.state.elements.lock().unwrap().clear();
                        }
                    });

                    ui.add(egui::Separator::default().spacing(8.0));

                    // Printer status toggles
                    {
                        let mut status = self.state.printer_status.lock().unwrap();
                        let r = egui::Rounding::same(rounding);

                        let (paper_color, paper_label) = if status.paper_present {
                            (egui::Color32::from_rgb(200, 240, 200), "Paper: OK")
                        } else {
                            (egui::Color32::from_rgb(255, 180, 180), "Paper: OUT")
                        };
                        let paper_btn = egui::Button::new(
                            egui::RichText::new(paper_label).small(),
                        )
                        .fill(paper_color)
                        .rounding(r)
                        .min_size(egui::vec2(0.0, widget_height));
                        if ui.add(paper_btn).clicked() {
                            status.paper_present = !status.paper_present;
                        }

                        let (cover_color, cover_label) = if status.cover_closed {
                            (egui::Color32::from_rgb(200, 240, 200), "Cover: Closed")
                        } else {
                            (egui::Color32::from_rgb(255, 180, 180), "Cover: Open")
                        };
                        let cover_btn = egui::Button::new(
                            egui::RichText::new(cover_label).small(),
                        )
                        .fill(cover_color)
                        .rounding(r)
                        .min_size(egui::vec2(0.0, widget_height));
                        if ui.add(cover_btn).clicked() {
                            status.cover_closed = !status.cover_closed;
                        }

                        let (drawer_color, drawer_label) = if status.drawer_open {
                            (egui::Color32::from_rgb(255, 220, 150), "Drawer: Open")
                        } else {
                            (egui::Color32::from_rgb(200, 240, 200), "Drawer: Closed")
                        };
                        let drawer_btn = egui::Button::new(
                            egui::RichText::new(drawer_label).small(),
                        )
                        .fill(drawer_color)
                        .rounding(r)
                        .min_size(egui::vec2(0.0, widget_height));
                        if ui.add(drawer_btn).clicked() {
                            status.drawer_open = !status.drawer_open;
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(
                            egui::Color32::DARK_GRAY,
                            format!("{}cpl | :{}", current_paper_size.chars_per_line(), self.state.port),
                        );

                        if let Some(ref status_msg) = self.export_status {
                            ui.colored_label(
                                egui::Color32::from_rgb(0, 140, 0),
                                egui::RichText::new(status_msg.as_str()).small(),
                            );
                        }

                        let export_btn = egui::Button::new(
                            egui::RichText::new("Export PNG").small(),
                        )
                        .fill(neutral_bg)
                        .rounding(egui::Rounding::same(rounding))
                        .min_size(egui::vec2(0.0, widget_height));
                        if ui.add(export_btn).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
                        }

                        let inspector_label = if self.inspector_open {
                            "Inspector ON"
                        } else {
                            "Inspector"
                        };
                        let inspector_color = if self.inspector_open {
                            egui::Color32::from_rgb(200, 220, 255)
                        } else {
                            neutral_bg
                        };
                        let inspector_btn = egui::Button::new(
                            egui::RichText::new(inspector_label).small(),
                        )
                        .fill(inspector_color)
                        .rounding(egui::Rounding::same(rounding))
                        .min_size(egui::vec2(0.0, widget_height));
                        if ui.add(inspector_btn).clicked() {
                            self.inspector_open = !self.inspector_open;
                        }
                    });
                });
            });

        // Clear receipt when paper size changes
        if paper_size_changed {
            self.state.elements.lock().unwrap().clear();
        }

        // Command inspector side panel
        if self.inspector_open {
            egui::SidePanel::right("inspector")
                .default_width(280.0)
                .min_width(200.0)
                .frame(
                    egui::Frame::none()
                        .fill(egui::Color32::from_gray(250))
                        .inner_margin(6.0)
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(200))),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Command Inspector");
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.small_button("Clear").clicked() {
                                    self.state.command_log.lock().unwrap().clear();
                                }
                            },
                        );
                    });
                    ui.separator();

                    let log = self.state.command_log.lock().unwrap();
                    let row_height = 16.0;
                    let total_rows = log.len();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false; 2])
                        .stick_to_bottom(true)
                        .show_rows(ui, row_height, total_rows, |ui, row_range| {
                            for idx in row_range {
                                if let Some(entry) = log.get(idx) {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!("{:4}", idx + 1))
                                                .small()
                                                .color(egui::Color32::GRAY),
                                        );
                                        ui.label(
                                            egui::RichText::new(&entry.hex)
                                                .small()
                                                .monospace()
                                                .color(egui::Color32::from_rgb(0, 100, 180)),
                                        );
                                        ui.label(
                                            egui::RichText::new(&entry.description)
                                                .small(),
                                        );
                                    });
                                }
                            }
                        });
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_gray(245)))
            .show(ctx, |ui| {
                let connections = self.state.connections.lock().unwrap();
                if !connections.is_empty() {
                    ui.label(format!("Active connections: {}", connections.len()));
                    for conn in connections.iter() {
                        ui.label(conn);
                    }
                    ui.separator();
                }
                drop(connections);

                // Fixed width scroll area matching 80mm receipt paper
                let printer_width_px = current_paper_size.width_px();
                let printer_chars_per_line = current_paper_size.chars_per_line();

                // Center the receipt area horizontally
                ui.vertical_centered(|ui| {
                    ui.set_width(printer_width_px + 2.0); // +2 for border

                    // Receipt paper frame with border
                    egui::Frame::none()
                        .fill(egui::Color32::WHITE)
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(200)))
                        .inner_margin(0.0)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false; 2])
                                .max_height(ui.available_height())
                                .show(ui, |ui| {
                                    ui.set_width(printer_width_px);
                                    let elements = self.state.elements.lock().unwrap();

                                    if elements.is_empty() {
                                        ui.add_space(100.0);
                                        ui.vertical_centered(|ui| {
                                            ui.colored_label(
                                                egui::Color32::DARK_GRAY,
                                                "Receipt empty",
                                            );
                                            ui.add_space(10.0);
                                            ui.colored_label(
                                                egui::Color32::GRAY,
                                                "Send print job to port 9100",
                                            );
                                            if paper_size_changed {
                                                ui.add_space(5.0);
                                                ui.colored_label(
                                                    egui::Color32::from_rgb(200, 150, 0),
                                                    format!(
                                                        "Paper size changed to {}",
                                                        current_paper_size.label()
                                                    ),
                                                );
                                            }
                                        });
                                    }

                                    for element in elements.iter() {
                                        match element {
                                            ReceiptElement::Text {
                                                content,
                                                bold,
                                                underline,
                                                double_width,
                                                double_height,
                                                inverted,
                                                alignment,
                                                density,
                                                offset,
                                                left_margin,
                                                character_spacing,
                                                double_strike,
                                                font,
                                                print_area_width,
                                            } => {
                                                let mut job = egui::text::LayoutJob::default();

                                                // Use print_area_width (GS W) for content sizing
                                                // when set, otherwise fall back to full printer width
                                                let effective_width = if *print_area_width > 0 {
                                                    *print_area_width as f32
                                                } else {
                                                    printer_width_px
                                                };

                                                // Calculate font size to fit chars per line
                                                // Measure actual monospace advance width ratio
                                                let char_width =
                                                    effective_width / printer_chars_per_line as f32;
                                                let ref_size = 20.0_f32;
                                                let ref_galley = ui.fonts(|f| {
                                                    f.layout_job(
                                                        egui::text::LayoutJob::simple_singleline(
                                                            "M".to_string(),
                                                            egui::FontId::monospace(ref_size),
                                                            egui::Color32::BLACK,
                                                        ),
                                                    )
                                                });
                                                let mono_ratio = ref_galley.size().x / ref_size;
                                                let base_font_size = char_width / mono_ratio;

                                                // Apply font selection (Font B is ~75% of Font A size)
                                                let font_multiplier = match font {
                                                    1 => 0.75, // Font B - smaller
                                                    2 => 0.65, // Font C - even smaller (if used)
                                                    _ => 1.0,  // Font A - standard
                                                };

                                                let mut size = base_font_size * font_multiplier;
                                                if *double_width || *double_height {
                                                    size = base_font_size * font_multiplier * 1.5;
                                                }

                                                // Always use monospace for consistent character widths
                                                // ESC/POS printers use fixed-width fonts
                                                // Bold will be rendered by egui's text rendering (stroke weight)
                                                let font_id = egui::FontId::monospace(size);

                                                // Apply bold, double-strike, and density
                                                let color = if *inverted {
                                                    egui::Color32::WHITE
                                                } else {
                                                    // Bold or double-strike makes text darker
                                                    if *bold || *double_strike {
                                                        egui::Color32::BLACK
                                                    } else {
                                                        match density {
                                                            0 => egui::Color32::LIGHT_GRAY,
                                                            1 => egui::Color32::GRAY,
                                                            2 => egui::Color32::DARK_GRAY,
                                                            _ => egui::Color32::BLACK, // 3-8: normal black
                                                        }
                                                    }
                                                };

                                                let bg_color = if *inverted {
                                                    egui::Color32::BLACK
                                                } else {
                                                    egui::Color32::TRANSPARENT
                                                };

                                                // Apply character spacing (ESC SP)
                                                let extra_letter_spacing =
                                                    *character_spacing as f32;

                                                job.append(
                                                    content,
                                                    0.0,
                                                    egui::TextFormat {
                                                        font_id,
                                                        color,
                                                        background: bg_color,
                                                        underline: if *underline {
                                                            egui::Stroke::new(1.0, color)
                                                        } else {
                                                            egui::Stroke::NONE
                                                        },
                                                        extra_letter_spacing,
                                                        ..Default::default()
                                                    },
                                                );

                                                let galley = ui.fonts(|f| f.layout_job(job));

                                                // Allocate full width for 80mm receipt paper
                                                let line_height = galley.size().y;

                                                let (rect, _) = ui.allocate_exact_size(
                                                    egui::vec2(printer_width_px, line_height),
                                                    egui::Sense::hover(),
                                                );

                                                // Apply left margin (GS L)
                                                let margin_offset = *left_margin as f32;

                                                // Center the printable area within the paper
                                                let area_offset = if *print_area_width > 0 {
                                                    (printer_width_px - *print_area_width as f32)
                                                        / 2.0
                                                } else {
                                                    0.0
                                                };

                                                // Calculate base position from alignment
                                                // All alignments use area_offset so content
                                                // stays within the GS W print area
                                                let base_x = match alignment {
                                                    Alignment::Left => {
                                                        rect.left() + area_offset + margin_offset
                                                    }
                                                    Alignment::Center => {
                                                        rect.left()
                                                            + area_offset
                                                            + margin_offset
                                                            + (effective_width
                                                                - galley.size().x
                                                                - margin_offset)
                                                                / 2.0
                                                    }
                                                    Alignment::Right => {
                                                        rect.left() + area_offset + effective_width
                                                            - galley.size().x
                                                    }
                                                };

                                                // Apply horizontal offset (from ESC $ / ESC \ commands)
                                                // Offset is in pixels, add to base position
                                                let final_x = if *offset > 0 {
                                                    rect.left() + margin_offset + *offset as f32
                                                } else {
                                                    base_x
                                                };

                                                let pos = egui::pos2(final_x, rect.top());

                                                ui.painter().galley(pos, galley, color);
                                            }
                                            ReceiptElement::RasterImage {
                                                width,
                                                height,
                                                data,
                                                offset,
                                                density,
                                                alignment,
                                                bytes_per_line,
                                                print_area_width,
                                            } => {
                                                render_raster_image(
                                                    ui,
                                                    *width,
                                                    *height,
                                                    data,
                                                    *offset,
                                                    *density,
                                                    alignment,
                                                    printer_width_px,
                                                    *bytes_per_line,
                                                    *print_area_width,
                                                );
                                            }
                                            ReceiptElement::QrCode {
                                                data,
                                                size,
                                                alignment,
                                                offset,
                                                print_area_width,
                                            } => {
                                                render_qr_code(
                                                    ui,
                                                    data,
                                                    *size,
                                                    alignment,
                                                    *offset,
                                                    *print_area_width,
                                                    printer_width_px,
                                                );
                                            }
                                            ReceiptElement::Barcode {
                                                data,
                                                barcode_type,
                                                height,
                                                module_width,
                                                hri_position,
                                                alignment,
                                                offset,
                                                print_area_width,
                                            } => {
                                                render_barcode(
                                                    ui,
                                                    data,
                                                    barcode_type,
                                                    *height,
                                                    *module_width,
                                                    hri_position,
                                                    alignment,
                                                    *offset,
                                                    *print_area_width,
                                                    printer_width_px,
                                                );
                                            }
                                            ReceiptElement::Barcode2D {
                                                data,
                                                variant,
                                                module_size,
                                                alignment,
                                                offset,
                                                print_area_width,
                                            } => {
                                                render_barcode_2d(
                                                    ui,
                                                    data,
                                                    variant,
                                                    *module_size,
                                                    alignment,
                                                    *offset,
                                                    *print_area_width,
                                                    printer_width_px,
                                                );
                                            }
                                            ReceiptElement::PaperCut { cut_type } => {
                                                ui.add_space(8.0);
                                                let (rect, _) = ui.allocate_exact_size(
                                                    egui::vec2(printer_width_px, 16.0),
                                                    egui::Sense::hover(),
                                                );
                                                let painter = ui.painter();
                                                let y = rect.center().y;
                                                let is_partial =
                                                    cut_type.contains("PARTIAL");
                                                let dash_len = if is_partial {
                                                    6.0
                                                } else {
                                                    10.0
                                                };
                                                let gap_len = 4.0;
                                                let color = if is_partial {
                                                    egui::Color32::from_gray(160)
                                                } else {
                                                    egui::Color32::from_gray(80)
                                                };

                                                let mut x = rect.left();
                                                while x < rect.right() {
                                                    let end =
                                                        (x + dash_len).min(rect.right());
                                                    painter.line_segment(
                                                        [
                                                            egui::pos2(x, y),
                                                            egui::pos2(end, y),
                                                        ],
                                                        egui::Stroke::new(1.5, color),
                                                    );
                                                    x += dash_len + gap_len;
                                                }

                                                let label = egui::RichText::new(format!(
                                                    "  {}  ",
                                                    cut_type
                                                ))
                                                .small()
                                                .color(color);
                                                let label_galley = ui.fonts(|f| {
                                                    f.layout_no_wrap(
                                                        format!("  {}  ", cut_type),
                                                        egui::FontId::proportional(9.0),
                                                        color,
                                                    )
                                                });
                                                let label_w = label_galley.size().x;
                                                let label_x = rect.center().x - label_w / 2.0;
                                                painter.rect_filled(
                                                    egui::Rect::from_min_size(
                                                        egui::pos2(label_x - 2.0, y - 6.0),
                                                        egui::vec2(label_w + 4.0, 12.0),
                                                    ),
                                                    0.0,
                                                    egui::Color32::WHITE,
                                                );
                                                painter.galley(
                                                    egui::pos2(label_x, y - 5.0),
                                                    label_galley,
                                                    color,
                                                );
                                                let _ = label;
                                                ui.add_space(8.0);
                                            }
                                            ReceiptElement::CashDrawer {
                                                pin,
                                                on_time,
                                                off_time,
                                            } => {
                                                ui.add_space(4.0);
                                                let (rect, _) = ui.allocate_exact_size(
                                                    egui::vec2(printer_width_px, 28.0),
                                                    egui::Sense::hover(),
                                                );
                                                let painter = ui.painter();
                                                let bg_color =
                                                    egui::Color32::from_rgb(255, 243, 220);
                                                let border_color =
                                                    egui::Color32::from_rgb(200, 160, 80);
                                                painter.rect(
                                                    rect,
                                                    2.0,
                                                    bg_color,
                                                    egui::Stroke::new(1.0, border_color),
                                                );
                                                let text = format!(
                                                    "CASH DRAWER  Pin:{}  On:{}ms  Off:{}ms",
                                                    pin,
                                                    *on_time as u32 * 2,
                                                    *off_time as u32 * 2
                                                );
                                                let galley = ui.fonts(|f| {
                                                    f.layout_no_wrap(
                                                        text,
                                                        egui::FontId::monospace(10.0),
                                                        egui::Color32::from_rgb(140, 100, 20),
                                                    )
                                                });
                                                let text_x = rect.center().x
                                                    - galley.size().x / 2.0;
                                                let text_y = rect.center().y
                                                    - galley.size().y / 2.0;
                                                painter.galley(
                                                    egui::pos2(text_x, text_y),
                                                    galley,
                                                    egui::Color32::from_rgb(140, 100, 20),
                                                );
                                                ui.add_space(4.0);
                                            }
                                            ReceiptElement::Separator => {
                                                ui.add_space(4.0);
                                            }
                                            ReceiptElement::FormFeed => {
                                                // Don't add artificial spacing - only show protocol breaks
                                            }
                                        }
                                    }
                                });
                        });
                });
            });
    }
}

#[allow(clippy::too_many_arguments)]
fn render_raster_image(
    ui: &mut egui::Ui,
    width: usize,
    height: usize,
    data: &[u8],
    offset: u16,
    density: u8,
    alignment: &Alignment,
    printer_width_px: f32,
    bytes_per_line: usize,
    print_area_width: u16,
) {
    // Use the actual bytes_per_line from the command, not recalculated
    let mut pixels = Vec::with_capacity(width * height);

    // Apply density/darkness control to raster images
    // Density 0-8 maps to different gray levels for lighter/darker printing
    let ink_color = match density {
        0 => egui::Color32::from_gray(180), // Very light
        1 => egui::Color32::from_gray(130), // Light
        2 => egui::Color32::from_gray(80),  // Slightly light
        _ => egui::Color32::BLACK,          // 3-8: normal black
    };

    for y in 0..height {
        for x in 0..width {
            let byte_idx = y * bytes_per_line + (x / 8);
            // MSB-first bit order: bit 7 (0x80) is leftmost pixel, bit 0 (0x01) is rightmost
            let bit_idx = 7 - (x % 8);

            if byte_idx < data.len() {
                let bit = (data[byte_idx] >> bit_idx) & 1;
                // Standard ESC/POS: 1=black (printed), 0=white (not printed)
                if bit == 1 {
                    pixels.push(ink_color); // Bit 1 = black
                } else {
                    pixels.push(egui::Color32::WHITE); // Bit 0 = white
                }
            } else {
                pixels.push(egui::Color32::WHITE);
            }
        }
    }

    let image = egui::ColorImage {
        size: [width, height],
        pixels,
    };

    let texture = ui.ctx().load_texture(
        format!("raster_{}x{}_{}", width, height, offset),
        image,
        egui::TextureOptions::NEAREST,
    );

    // Use print_area_width (GS W) for alignment when set,
    // otherwise fall back to full printer width
    let effective_width = if print_area_width > 0 {
        print_area_width as f32
    } else {
        printer_width_px
    };

    // Scale up the image for better visibility (thermal printers are 203 DPI, screens are ~96 DPI)
    // Use adaptive scaling: small images (text) get 3x, large images (logos) get 1x
    // Clamp so the image never exceeds the printable area
    let scale_factor = if width > 300 || height > 150 {
        1.0
    } else {
        3.0_f32.min(effective_width / width as f32)
    };
    let display_width = width as f32 * scale_factor;
    let display_height = height as f32 * scale_factor;

    // Allocate full printer width for proper alignment
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(printer_width_px, display_height),
        egui::Sense::hover(),
    );

    // Center the printable area within the paper width
    let area_offset = if print_area_width > 0 {
        (printer_width_px - print_area_width as f32) / 2.0
    } else {
        0.0
    };

    // Calculate horizontal position based on alignment and offset
    // For CENTER/RIGHT, center the printable area within the paper.
    // For LEFT, use left edge only.
    let x_offset = match alignment {
        Alignment::Left => offset as f32 * scale_factor,
        Alignment::Center => {
            area_offset + (effective_width - display_width) / 2.0 + offset as f32 * scale_factor
        }
        Alignment::Right => {
            area_offset + effective_width - display_width - offset as f32 * scale_factor
        }
    };

    let pos = egui::pos2(rect.left() + x_offset, rect.top());
    let size = egui::vec2(display_width, display_height);

    ui.painter().image(
        texture.id(),
        egui::Rect::from_min_size(pos, size),
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

fn render_qr_code(
    ui: &mut egui::Ui,
    data: &str,
    size: usize,
    alignment: &Alignment,
    offset: u16,
    print_area_width: u16,
    printer_width_px: f32,
) {
    match QrCode::new(data.as_bytes()) {
        Ok(qr) => {
            let colors = qr.to_colors();
            let width = qr.width();
            let module_size = size.clamp(1, 8);
            let pixel_size = width * module_size;

            let mut pixels = Vec::with_capacity(pixel_size * pixel_size);

            for y in 0..width {
                for _ in 0..module_size {
                    for x in 0..width {
                        let idx = y * width + x;
                        let color = match colors[idx] {
                            QrColor::Dark => egui::Color32::BLACK,
                            QrColor::Light => egui::Color32::WHITE,
                        };
                        for _ in 0..module_size {
                            pixels.push(color);
                        }
                    }
                }
            }

            let image = egui::ColorImage {
                size: [pixel_size, pixel_size],
                pixels,
            };

            let texture = ui.ctx().load_texture(
                format!("qr_{}", data.chars().take(20).collect::<String>()),
                image,
                egui::TextureOptions::NEAREST,
            );

            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(printer_width_px, pixel_size as f32),
                egui::Sense::hover(),
            );

            // Use print_area_width (GS W) for alignment when set,
            // otherwise fall back to full printer width
            let effective_width = if print_area_width > 0 {
                print_area_width as f32
            } else {
                printer_width_px
            };

            // Center the printable area within the paper width
            let area_offset = if print_area_width > 0 {
                (printer_width_px - print_area_width as f32) / 2.0
            } else {
                0.0
            };

            // Calculate base position from alignment
            // For CENTER/RIGHT, center the printable area within the paper.
            // For LEFT, use left edge only.
            let base_x = match alignment {
                Alignment::Left => 0.0,
                Alignment::Center => area_offset + (effective_width - pixel_size as f32) / 2.0,
                Alignment::Right => area_offset + effective_width - pixel_size as f32,
            };

            // Apply horizontal offset (from ESC $ / ESC \ commands)
            let final_x = if offset > 0 { offset as f32 } else { base_x };

            let pos = egui::pos2(rect.left() + final_x, rect.top());
            let size = egui::vec2(pixel_size as f32, pixel_size as f32);

            ui.painter().image(
                texture.id(),
                egui::Rect::from_min_size(pos, size),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        Err(e) => {
            ui.colored_label(egui::Color32::RED, format!("QR Code Error: {:?}", e));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_barcode(
    ui: &mut egui::Ui,
    data: &[u8],
    barcode_type: &BarcodeType,
    height: u8,
    module_width: u8,
    hri_position: &HriPosition,
    alignment: &Alignment,
    offset: u16,
    print_area_width: u16,
    printer_width_px: f32,
) {
    let data_str = String::from_utf8_lossy(data);
    let format = barcode_type.to_rxing_format();

    let writer = MultiFormatWriter;
    let bit_matrix = match writer.encode(&data_str, &format, 0, 0) {
        Ok(matrix) => matrix,
        Err(e) => {
            ui.colored_label(
                egui::Color32::RED,
                format!("Barcode Error ({:?}): {}", barcode_type, e),
            );
            return;
        }
    };

    let bar_width = bit_matrix.width() as usize;
    let scale = module_width as usize;
    let pixel_width = bar_width * scale;
    let pixel_height = height as usize;

    let hri_line_height: usize = 14;
    let above_hri = matches!(hri_position, HriPosition::Above | HriPosition::Both);
    let below_hri = matches!(hri_position, HriPosition::Below | HriPosition::Both);
    let hri_above_h = if above_hri { hri_line_height } else { 0 };
    let hri_below_h = if below_hri { hri_line_height } else { 0 };
    let total_height = pixel_height + hri_above_h + hri_below_h;

    let mut pixels = Vec::with_capacity(pixel_width * total_height);

    // White space for HRI above
    for _ in 0..(pixel_width * hri_above_h) {
        pixels.push(egui::Color32::WHITE);
    }

    // Barcode bars
    for _y in 0..pixel_height {
        for x in 0..bar_width {
            let color = if bit_matrix.get(x as u32, 0) {
                egui::Color32::BLACK
            } else {
                egui::Color32::WHITE
            };
            for _ in 0..scale {
                pixels.push(color);
            }
        }
    }

    // White space for HRI below
    for _ in 0..(pixel_width * hri_below_h) {
        pixels.push(egui::Color32::WHITE);
    }

    let image = egui::ColorImage {
        size: [pixel_width, total_height],
        pixels,
    };

    let texture = ui.ctx().load_texture(
        format!(
            "barcode_{:?}_{}",
            barcode_type,
            data_str.chars().take(20).collect::<String>()
        ),
        image,
        egui::TextureOptions::NEAREST,
    );

    let effective_width = if print_area_width > 0 {
        print_area_width as f32
    } else {
        printer_width_px
    };

    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(printer_width_px, total_height as f32),
        egui::Sense::hover(),
    );

    let area_offset = if print_area_width > 0 {
        (printer_width_px - print_area_width as f32) / 2.0
    } else {
        0.0
    };

    let base_x = match alignment {
        Alignment::Left => 0.0,
        Alignment::Center => area_offset + (effective_width - pixel_width as f32) / 2.0,
        Alignment::Right => area_offset + effective_width - pixel_width as f32,
    };

    let final_x = if offset > 0 { offset as f32 } else { base_x };

    let pos = egui::pos2(rect.left() + final_x, rect.top());
    let size = egui::vec2(pixel_width as f32, total_height as f32);

    ui.painter().image(
        texture.id(),
        egui::Rect::from_min_size(pos, size),
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    // Render HRI text overlay
    if *hri_position != HriPosition::None {
        let font_id = egui::FontId::monospace(10.0);
        let text_galley =
            ui.fonts(|f| f.layout_no_wrap(data_str.to_string(), font_id, egui::Color32::BLACK));
        let text_width = text_galley.size().x;
        let text_x = pos.x + (pixel_width as f32 - text_width) / 2.0;

        if above_hri {
            ui.painter().galley(
                egui::pos2(text_x, pos.y + 1.0),
                text_galley.clone(),
                egui::Color32::BLACK,
            );
        }
        if below_hri {
            ui.painter().galley(
                egui::pos2(text_x, pos.y + (total_height - hri_line_height) as f32 + 1.0),
                text_galley,
                egui::Color32::BLACK,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_barcode_2d(
    ui: &mut egui::Ui,
    data: &[u8],
    variant: &Barcode2DVariant,
    module_size: u8,
    alignment: &Alignment,
    offset: u16,
    print_area_width: u16,
    printer_width_px: f32,
) {
    let format = match variant {
        Barcode2DVariant::Pdf417 => BarcodeFormat::PDF_417,
        Barcode2DVariant::DataMatrix => BarcodeFormat::DATA_MATRIX,
    };

    let data_str = String::from_utf8_lossy(data);
    let writer = MultiFormatWriter;
    let bit_matrix = match writer.encode(&data_str, &format, 0, 0) {
        Ok(matrix) => matrix,
        Err(e) => {
            ui.colored_label(
                egui::Color32::RED,
                format!("{:?} Error: {}", variant, e),
            );
            return;
        }
    };

    let w = bit_matrix.width() as usize;
    let h = bit_matrix.height() as usize;
    let scale = module_size.max(1) as usize;
    let pixel_width = w * scale;
    let pixel_height = h * scale;

    let mut pixels = Vec::with_capacity(pixel_width * pixel_height);
    for y in 0..h {
        for _ in 0..scale {
            for x in 0..w {
                let color = if bit_matrix.get(x as u32, y as u32) {
                    egui::Color32::BLACK
                } else {
                    egui::Color32::WHITE
                };
                for _ in 0..scale {
                    pixels.push(color);
                }
            }
        }
    }

    let image = egui::ColorImage {
        size: [pixel_width, pixel_height],
        pixels,
    };

    let texture = ui.ctx().load_texture(
        format!(
            "{:?}_{}",
            variant,
            data_str.chars().take(20).collect::<String>()
        ),
        image,
        egui::TextureOptions::NEAREST,
    );

    let effective_width = if print_area_width > 0 {
        print_area_width as f32
    } else {
        printer_width_px
    };

    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(printer_width_px, pixel_height as f32),
        egui::Sense::hover(),
    );

    let area_offset = if print_area_width > 0 {
        (printer_width_px - print_area_width as f32) / 2.0
    } else {
        0.0
    };

    let base_x = match alignment {
        Alignment::Left => 0.0,
        Alignment::Center => area_offset + (effective_width - pixel_width as f32) / 2.0,
        Alignment::Right => area_offset + effective_width - pixel_width as f32,
    };

    let final_x = if offset > 0 { offset as f32 } else { base_x };

    let pos = egui::pos2(rect.left() + final_x, rect.top());
    let size = egui::vec2(pixel_width as f32, pixel_height as f32);

    ui.painter().image(
        texture.id(),
        egui::Rect::from_min_size(pos, size),
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

async fn handle_client(
    mut socket: tokio::net::TcpStream,
    addr: std::net::SocketAddr,
    state: AppState,
    debug: bool,
) -> Result<()> {
    {
        let mut connections = state.connections.lock().unwrap();
        connections.push(format!("Connected: {}", addr));
    }

    let mut renderer = EscPosRenderer::new(
        debug,
        state.printer_status.clone(),
        state.nv_images.clone(),
        state.command_log.clone(),
    );
    let mut buffer = vec![0u8; 8192];

    // Open file for raw data capture if debug enabled
    let mut raw_file = if debug {
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open("escpos_capture.raw")
            .ok()
    } else {
        None
    };

    loop {
        match socket.read(&mut buffer).await {
            Ok(0) => {
                let mut connections = state.connections.lock().unwrap();
                connections.retain(|c| !c.contains(&addr.to_string()));
                break;
            }
            Ok(n) => {
                // Save raw data if debug enabled
                if let Some(ref mut file) = raw_file {
                    use std::io::Write;
                    let _ = file.write_all(&buffer[..n]);
                }

                if debug {
                    eprintln!("[DEBUG] Received {} bytes: {:02X?}", n, &buffer[..n]);
                }

                if let Err(e) = renderer.process_data(&buffer[..n]) {
                    eprintln!("Error processing data: {}", e);
                }

                // Send any queued responses (status queries, etc.)
                let responses = renderer.take_responses();
                if !responses.is_empty() {
                    if debug {
                        eprintln!(
                            "[DEBUG] Sending {} response bytes: {:02X?}",
                            responses.len(),
                            responses
                        );
                    }
                    if let Err(e) = socket.write_all(&responses).await {
                        eprintln!("Error sending responses: {}", e);
                    }
                    if let Err(e) = socket.flush().await {
                        eprintln!("Error flushing socket: {}", e);
                    }
                }

                let new_elements = renderer.take_elements();
                if !new_elements.is_empty() {
                    let mut elements = state.elements.lock().unwrap();
                    elements.extend(new_elements);
                }
            }
            Err(e) => {
                eprintln!("Error reading from socket: {}", e);
                break;
            }
        }
    }

    Ok(())
}

#[derive(Parser)]
#[command(name = "escpresso", about = "Virtual ESC/POS thermal receipt printer emulator")]
struct Args {
    #[arg(short, long, default_value_t = 9100)]
    port: u16,

    #[arg(short, long)]
    debug: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let debug = args.debug || std::env::var("DEBUG").is_ok();
    let port = args.port;
    let state = AppState::new(port);
    let state_clone = state.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
                Ok(listener) => listener,
                Err(e) => {
                    eprintln!("ERROR: Failed to bind to port {}: {}", port, e);
                    eprintln!("Port {} is already in use. Please:", port);
                    eprintln!("  1. Stop any other escpresso instances");
                    eprintln!("  2. Check for other applications using port {}:", port);
                    eprintln!("     lsof -i :{}", port);
                    std::process::exit(1);
                }
            };
            println!("TCP Server listening on 0.0.0.0:{}", port);
            if debug {
                eprintln!("[DEBUG] Debug mode enabled");
            }

            loop {
                match listener.accept().await {
                    Ok((socket, addr)) => {
                        let state = state_clone.clone();
                        let debug_flag = debug;
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(socket, addr, state, debug_flag).await {
                                eprintln!("Error handling client {}: {}", addr, e);
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("Error accepting connection: {}", e);
                    }
                }
            }
        });
    });

    let default_width = PaperSize::Size80mm.width_px();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([default_width + 40.0, 800.0]) // Receipt width + padding
            .with_title("escpresso"),
        ..Default::default()
    };

    eframe::run_native(
        "escpresso",
        options,
        Box::new(move |cc| Ok(Box::new(VirtualEscPosApp::new(cc, state)))),
    )
    .map_err(|e| anyhow::anyhow!("Failed to run app: {}", e))
}
