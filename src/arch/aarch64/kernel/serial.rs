use alloc::collections::vec_deque::VecDeque;
use core::num::NonZero;
use core::ptr::NonNull;

use arm_pl011_uart::{DataBits, Interrupts, LineConfig, Parity, StopBits, Uart, UniqueMmioPointer};
use embedded_io::{ErrorType, Read, ReadReady, Write};
use hermit_sync::{InterruptTicketMutex, Lazy};

use crate::errno::Errno;

/// The PL011, or `None` when the loader passed no serial port: some
/// hypervisors (Apple's Virtualization framework) have no UART, and a store
/// to an unbacked address stops the VM.
static UART_DEVICE: Lazy<InterruptTicketMutex<Option<UartDevice>>> =
	Lazy::new(|| InterruptTicketMutex::new(UartDevice::new()));

/// Output written while there is no UART, kept until a virtio console takes
/// over (see [`take_early_output`]). Output past its capacity is dropped.
static EARLY_OUTPUT: InterruptTicketMutex<EarlyOutput> =
	InterruptTicketMutex::new(EarlyOutput::new());

struct EarlyOutput {
	buf: [u8; Self::CAPACITY],
	len: usize,
}

impl EarlyOutput {
	const CAPACITY: usize = 16 * 1024;

	const fn new() -> Self {
		Self {
			buf: [0; Self::CAPACITY],
			len: 0,
		}
	}

	fn push(&mut self, bytes: &[u8]) {
		let n = bytes.len().min(Self::CAPACITY - self.len);
		self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
		self.len += n;
	}
}

/// Keeps `bytes` for a console that cannot write yet.
pub(crate) fn push_early_output(bytes: &[u8]) {
	EARLY_OUTPUT.lock().push(bytes);
}

/// Hands `f` the output written before any console device could write it,
/// then discards it. Called by the virtio console's first working write.
pub(crate) fn take_early_output(f: impl FnOnce(&[u8])) {
	let mut early = EARLY_OUTPUT.lock();
	let len = early.len;
	f(&early.buf[..len]);
	early.len = 0;
}

pub(crate) struct UartDevice {
	uart: Uart<'static>,
	buffer: VecDeque<u8>,
}

impl UartDevice {
	pub fn new() -> Option<Self> {
		let base = crate::env::boot_info().hardware_info.serial_port_base?;
		let base = NonZero::try_from(base).unwrap();
		let base = NonNull::with_exposed_provenance(base);

		let uart_pointer = unsafe { UniqueMmioPointer::new(base) };

		let mut uart = Uart::new(uart_pointer);

		let line_config = LineConfig {
			data_bits: DataBits::Bits8,
			parity: Parity::None,
			stop_bits: StopBits::One,
		};
		uart.enable(line_config, 115_200, 16_000_000).unwrap();

		uart.set_interrupt_masks(Interrupts::RXI | Interrupts::RTI);
		uart.clear_interrupts(Interrupts::all());

		Some(Self {
			uart,
			buffer: VecDeque::new(),
		})
	}
}

pub(crate) struct SerialDevice;

impl SerialDevice {
	pub fn new() -> Self {
		Self
	}
}

impl ErrorType for SerialDevice {
	type Error = Errno;
}

impl Read for SerialDevice {
	fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
		match UART_DEVICE.lock().as_mut() {
			Some(device) => Ok(device.buffer.read(buf)?),
			None => Ok(0),
		}
	}
}

impl ReadReady for SerialDevice {
	fn read_ready(&mut self) -> Result<bool, Self::Error> {
		Ok(UART_DEVICE
			.lock()
			.as_ref()
			.is_some_and(|device| !device.buffer.is_empty()))
	}
}

impl Write for SerialDevice {
	fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
		let mut guard = UART_DEVICE.lock();

		match guard.as_mut() {
			Some(device) => {
				for byte in buf {
					device.uart.write_word(*byte);
				}
			}
			None => push_early_output(buf),
		}

		Ok(buf.len())
	}

	fn flush(&mut self) -> Result<(), Self::Error> {
		Ok(())
	}
}

pub(crate) fn handle_uart_interrupt() {
	let mut guard = UART_DEVICE.lock();
	let Some(device) = guard.as_mut() else {
		return;
	};

	while let Ok(Some(mut byte)) = device.uart.read_word() {
		// Normalize CR to LF
		if byte == b'\r' {
			byte = b'\n';
		}

		device.buffer.push_back(byte);
	}

	device
		.uart
		.clear_interrupts(Interrupts::RXI | Interrupts::RTI);

	drop(guard);

	crate::console::CONSOLE_WAKER.lock().wake();
}
