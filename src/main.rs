use std::thread;
use std::sync::mpsc;
//use std::io::{self};
use std::fs::File;
use std::thread::JoinHandle;
use std::time::Duration;
use timeout_readwrite::TimeoutReader;
use std::os::unix::io::AsRawFd;
use std::io::Error;
use std::sync::mpsc::Sender;
use termios::*;
use termios::os::linux::*;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::env;
use std::os::unix::io::FromRawFd;
use log::{info, warn, trace, error};
use std::io::ErrorKind;
use crate::libc::c_int;
use std::os::unix::io::RawFd;
use gpio::GpioOut;
use std::io::Write;
use std::clone::Clone;

extern crate ioctl_rs as ioctl;
extern crate termios;
extern crate nix;
#[cfg(not(test))]
extern crate libc;

enum SpiMessage {
    Poll {
    },
    DmdFrame {
        data: [u8; 2048]
    },
    BacklightLed {
        brightness: u8
    },
    ReadLocalSwitches,
    Exit
}

#[derive(Clone)]
enum SpikeVersion {
    Spike1,
    Spike2,
}

enum PollResult {
    Unknown,
    Dirty {
        result: u8
    },
    Clean
}

enum NodeBusMessage {
    BusMessage {
        node: u8,
        len: u8,
        cmd: u8,
        data: [u8; 256],
        checksum: u8,
        response_len: u8
    },
    BridgeMessage {
        cmd: u8,
        len: u8,
        data: [u8; 256],
        response_len: u8
    },
    Poll,
    Wait {
        wait_ms: u8
    },
    Exit
}

enum Response {
    Message {
        data: [u8; 256],
        len: u8
    },
    Poll {
        result: u8
    },
    SpiPoll {
        dirty: bool
    },
    Exit
}

#[cfg(not(test))]
mod ioctl_custom {
    use nix::*;

    #[allow(dead_code)]
    const GPIO_STATE: u16 = 0x3C02;

    const GPIO_ON: u16 = 0x3C03;
    const GPIO_OFF: u16 = 0x3C04;

    const BACKLIGHT_MAGIC: u16 = 0x4001;

    ioctl_write_ptr_bad!(set_brightness, BACKLIGHT_MAGIC, libc::c_int);

    #[allow(dead_code)]
    ioctl_read_bad!(get_gpio_state, GPIO_STATE, bool);

    ioctl_write_int_bad!(set_gpio_on, GPIO_ON);
    ioctl_write_int_bad!(set_gpio_off, GPIO_OFF);

    ioctl_read_bad!(fionread, libc::FIONREAD, libc::c_int);
}

#[cfg(test)]
#[macro_use(lazy_static)]
extern crate lazy_static;

#[cfg(test)]
mod ioctl_custom {
    use nix::Error;


    use std::sync::{Arc, Mutex};

    lazy_static! {
        pub static ref BACKLIGHT_BRIGHTNESS: Arc<Mutex<i32>> =
            Arc::new(Mutex::new(0));
        pub static ref IOCTL_LOCK: Arc<Mutex<i32>> =
            Arc::new(Mutex::new(0));
}

    pub unsafe fn set_brightness(_fd: i32, _value: &libc::c_int) -> Result<(), Error> {
        let mut data = BACKLIGHT_BRIGHTNESS.lock().unwrap();
        *data = *_value;
        Ok(())
    }

    #[allow(dead_code)]
    pub unsafe fn get_gpio_state(_fd: i32, _value: &mut libc::c_int) -> Result<(), Error> { Ok(()) }

    pub unsafe fn set_gpio_on(_fd: i32, _value: libc::c_int) -> Result<(), Error> {
        Ok(())
    }

    pub unsafe fn set_gpio_off(_fd: i32, _value: libc::c_int) -> Result<(), Error> {
        Ok(())
    }

    pub unsafe fn fionread(_fd: i32, _value: &mut libc::c_int) -> Result<(), Error> { Ok(() )
    }
}

pub fn fionread(fd: RawFd) -> Result<u32, nix::Error> {
    let mut retval: libc::c_int = 0;
    unsafe { ioctl_custom::fionread(fd, &mut retval) }
        .map(|_| retval as u32)
        .map_err(|e| e.into())
}

#[cfg(test)]
mod libc {
    pub const O_NOCTTY: i32 = 1;
    pub const O_SYNC: i32 = 2;
    #[allow(non_camel_case_types)]
    pub type c_int = i32;

    //pub unsafe fn tcdrain(_fd: i32) {}
    pub unsafe fn tcflush(_fd: i32, _action: i32) {}
    pub unsafe fn tcsendbreak(_fd: i32, _duration: i32) -> i32 {0}

}

#[cfg(test)]
mod tests {
    use crate::run_threads;
    use std::sync::mpsc::Sender;
    use std::sync::mpsc::Receiver;
    use std::io::Write;
    use std::io::Read;
    use std::io::Error;
    use std::sync::mpsc;
    use std::time::Duration;
    use std::fs::File;
    use std::os::raw::c_int;
    use std::sync::mpsc::RecvTimeoutError;
    use std::os::unix::io::AsRawFd;
    use crate::ioctl_custom;
    use crate::SpikeVersion;

    struct TestPipeSender {
        sender: Sender<u8>
    }

    impl Write for TestPipeSender {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Error> {
            let len = buf.len();
            for i in (std::ops::Range { start: 0, end: len }) {
                self.sender.send(buf[i]).unwrap();
            }
            Ok(len)
        }

        fn flush(&mut self) -> Result<(), Error> {
            Ok(())
        }

    }

    struct TestPipeReader {
        receiver: Receiver<u8>
    }

    impl Read for TestPipeReader {

        fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
            let len = buf.len();
            for i in (std::ops::Range { start: 0, end: len }) {
                buf[i] = self.receiver.recv().unwrap();
            }
            Ok(len)
        }
    }

    impl std::os::unix::io::AsRawFd for TestPipeReader {

        fn as_raw_fd(&self) -> c_int {
            return -1;
        }

    }

    impl std::os::unix::io::AsRawFd for TestPipeSender {

        fn as_raw_fd(&self) -> c_int {
            return -1;
        }

    }

    fn write_to_pipe(pipe: &Sender<u8>, data: Vec<u8>) {
        for v in data {
            pipe.send(v).unwrap();
        }
    }

    fn read_from_pipe(pipe: &Receiver<u8>, len: usize) -> Vec<u8> {
        let mut data = Vec::new();
        for _ in (std::ops::Range { start: 0, end: len }) {
            match pipe.recv_timeout(Duration::new(5, 0)) {
                Ok(value) => {data.push(value);},
                Err(RecvTimeoutError::Timeout) => {panic!("Got a timeout from thread")}
                Err(RecvTimeoutError::Disconnected) => {panic!("Thread disconnected")}
            }
        }
        return data;
    }

    #[test]
    fn integration_test_spike1() {
        //stderrlog::new().module(module_path!()).init().unwrap();
        let (host_in_tx, host_in_rx) = mpsc::channel::<u8>();
        let host_fd_in = TestPipeReader{ receiver: host_in_rx };

        let (host_out_tx, host_out_rx) = mpsc::channel::<u8>();
        let host_fd_out = TestPipeSender{ sender: host_out_tx };

        let (bus_in_tx, bus_in_rx) = mpsc::channel::<u8>();
        let bus_fd_in = TestPipeReader{ receiver: bus_in_rx };
        let bus_fd_in_raw = bus_fd_in.as_raw_fd();

        let (bus_out_tx, bus_out_rx) = mpsc::channel::<u8>();
        let bus_fd_out = TestPipeSender{ sender: bus_out_tx };

        let (spi_in_tx, spi_in_rx) = mpsc::channel::<u8>();
        let spi_fd_in = TestPipeReader{ receiver: spi_in_rx };

        let (spi_out_tx, spi_out_rx) = mpsc::channel::<u8>();
        let dmd_fd_out = TestPipeSender{ sender: spi_out_tx };

        let backlight_fd = File::open("/dev/null").unwrap();

        let threads = run_threads(host_fd_in, host_fd_out, bus_fd_in, bus_fd_in_raw,
                                  bus_fd_out, spi_fd_in, Some(dmd_fd_out), Some(backlight_fd), SpikeVersion::Spike1{});

        // Test init
        write_to_pipe(&host_in_tx, vec![0x80, 0x02, 0xf1, 0x8d, 0x00]);
        let data = read_from_pipe(&bus_out_rx, 5);
        assert_eq!(data, [0x80, 0x02, 0xf1, 0x8d, 0x00]);

        // Send command to node (with response_len 12)
        write_to_pipe(&host_in_tx, vec![0x81, 0x02, 0xfe, 0x7f, 0x0c]);
        // Check that node got message
        let data = read_from_pipe(&bus_out_rx, 5);
        assert_eq!(data, [0x81, 0x02, 0xfe, 0x7f, 0x0c]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 12);
        assert_eq!(data, vec![0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        // Sleep
        write_to_pipe(&host_in_tx, vec![0x01, 0x10]);

        // Poll
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x00]);
        // SPI sends switches
        write_to_pipe(&spi_in_tx, vec![0x00; 8]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0x00]);

        // Poll (board 8 dirty)
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x08]);
        // SPI sends switches
        write_to_pipe(&spi_in_tx, vec![0x00; 8]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0x08]);

        // Poll (SPI dirty)
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x00]);
        // SPI sends switches (one changed)
        write_to_pipe(&spi_in_tx, vec![0x02, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0xF0]);

        // Poll (SPI and node dirty)
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x08]);
        // SPI sends switches (one changed)
        write_to_pipe(&spi_in_tx, vec![0x03, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0xF0]);

        // Read switches from SPI
        write_to_pipe(&host_in_tx, vec![0x80, 0x02, 0x11, 0xFF, 0x0B]);
        // SPI sends switches
        write_to_pipe(&spi_in_tx, vec![0x03, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response
        let data = read_from_pipe(&host_out_rx, 10);
        assert_eq!(data, [0x03, 0, 0, 0, 0, 0, 0, 0, 253, 0]);

        // Poll (SPI no longer dirty but node dirty)
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x08]);
        // SPI sends switches (one changed)
        write_to_pipe(&spi_in_tx, vec![0x03, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0x08]);

        {
            // Lock IOCTL mock to prevent other tests from breaking stuff
            let _guard  = ioctl_custom::IOCTL_LOCK.lock().unwrap();
            *ioctl_custom::BACKLIGHT_BRIGHTNESS.lock().unwrap() = 0;

            // Set backlight
            write_to_pipe(&host_in_tx, vec![0x80, 0x04, 0x80, 0x00, 0xff, 0xff, 0x00]);
            // Send command to node (with response_len 12) to make sure that everything was processed
            write_to_pipe(&host_in_tx, vec![0x81, 0x02, 0xfe, 0x7f, 0x0c]);
            // Check that node got message
            let data = read_from_pipe(&bus_out_rx, 5);
            assert_eq!(data, [0x81, 0x02, 0xfe, 0x7f, 0x0c]);
            // Node sends response
            write_to_pipe(&bus_in_tx, vec![0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            // Check that host got the response from the node
            let data = read_from_pipe(&host_out_rx, 12);
            assert_eq!(data, vec![0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            // Assert backlight
            assert_eq!(*ioctl_custom::BACKLIGHT_BRIGHTNESS.lock().unwrap(), 0xff00);

            // Set backlight via Spike 2 command
            write_to_pipe(&host_in_tx, vec![0x09, 0x02, 0xcc, 0x11, 0x00]);
            // Send command to node (with response_len 12) to make sure that everything was processed
            write_to_pipe(&host_in_tx, vec![0x81, 0x02, 0xfe, 0x7f, 0x0c]);
            // Check that node got message
            let data = read_from_pipe(&bus_out_rx, 5);
            assert_eq!(data, [0x81, 0x02, 0xfe, 0x7f, 0x0c]);
            // Node sends response
            write_to_pipe(&bus_in_tx, vec![0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            // Check that host got the response from the node
            let data = read_from_pipe(&host_out_rx, 12);
            assert_eq!(data, vec![0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            // Assert backlight
            assert_eq!(*ioctl_custom::BACKLIGHT_BRIGHTNESS.lock().unwrap(), 0xcc00);
        }

        // SetResponseTime
        write_to_pipe(&host_in_tx, vec![0x06, 0x02, 0x45, 0x03, 0x00]);
        // Check that node got message
        let data = read_from_pipe(&bus_out_rx, 4);
        assert_eq!(data, [0x06, 0x02, 0x45, 0x03]);
        // No response

        // GetBridgeVersion
        write_to_pipe(&host_in_tx, vec![0x03, 0x00, 0x03]);
        // Check that node got message
        let data = read_from_pipe(&bus_out_rx, 2);
        assert_eq!(data, [0x03, 0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x01, 0x00, 0x03]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 3);
        assert_eq!(data, vec![0x01, 0x00, 0x03]);

        // GetBridgeState
        write_to_pipe(&host_in_tx, vec![0x05, 0x00, 0x01]);
        // Check that node got message
        let data = read_from_pipe(&bus_out_rx, 2);
        assert_eq!(data, [0x05, 0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x18]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, vec![0x18]);

        // Send DMD frame
        let mut dmd_frame = vec![0; 2048];
        dmd_frame[3] = 42;
        dmd_frame[4] = 23;
        let mut message = vec![0x80, 0x00, 0x90];
        message.append(&mut dmd_frame.clone());
        write_to_pipe(&host_in_tx, message);
        let data = read_from_pipe(&spi_out_rx, 2048);
        assert_eq!(data, dmd_frame);

        // Quit
        write_to_pipe(&host_in_tx, vec![0xF5]);

        for thread in threads {
           match thread.join() {
               Ok(_) => {},
               Err(_) => {panic!("Thread crashed")},
           }
        }
    }

    #[test]
    fn integration_test_spike2() {
        //stderrlog::new().module(module_path!()).init().unwrap();
        let (host_in_tx, host_in_rx) = mpsc::channel::<u8>();
        let host_fd_in = TestPipeReader{ receiver: host_in_rx };

        let (host_out_tx, host_out_rx) = mpsc::channel::<u8>();
        let host_fd_out = TestPipeSender{ sender: host_out_tx };

        let (bus_in_tx, bus_in_rx) = mpsc::channel::<u8>();
        let bus_fd_in = TestPipeReader{ receiver: bus_in_rx };
        let bus_fd_in_raw = bus_fd_in.as_raw_fd();

        let (bus_out_tx, bus_out_rx) = mpsc::channel::<u8>();
        let bus_fd_out = TestPipeSender{ sender: bus_out_tx };

        let (spi_in_tx, spi_in_rx) = mpsc::channel::<u8>();
        let spi_fd_in = TestPipeReader{ receiver: spi_in_rx };

        let threads = run_threads(host_fd_in, host_fd_out, bus_fd_in, bus_fd_in_raw,
                                  bus_fd_out, spi_fd_in, None::<TestPipeSender>, None::<TestPipeSender>, SpikeVersion::Spike2{});

        // Test init
        write_to_pipe(&host_in_tx, vec![0x80, 0x02, 0xf1, 0x8d, 0x00]);
        let data = read_from_pipe(&bus_out_rx, 5);
        assert_eq!(data, [0x80, 0x02, 0xf1, 0x8d, 0x00]);

        // Send command to node (with response_len 12)
        write_to_pipe(&host_in_tx, vec![0x81, 0x02, 0xfe, 0x7f, 0x0c]);
        // Check that node got message
        let data = read_from_pipe(&bus_out_rx, 5);
        assert_eq!(data, [0x81, 0x02, 0xfe, 0x7f, 0x0c]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 12);
        assert_eq!(data, vec![0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

        // Sleep
        write_to_pipe(&host_in_tx, vec![0x01, 0x10]);

        // Poll
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x00]);
        // SPI sends switches
        write_to_pipe(&spi_in_tx, vec![0x00; 8]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0x00]);

        // Poll (board 8 dirty)
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x08]);
        // SPI sends switches
        write_to_pipe(&spi_in_tx, vec![0x00; 8]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0x08]);

        // Poll (SPI dirty)
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x00]);
        // SPI sends switches (one changed)
        write_to_pipe(&spi_in_tx, vec![0x02, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0xF0]);

        // Poll (SPI and node dirty)
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x08]);
        // SPI sends switches (one changed)
        write_to_pipe(&spi_in_tx, vec![0x03, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0xF0]);

        // Read switches from SPI
        write_to_pipe(&host_in_tx, vec![0x80, 0x02, 0x11, 0xFF, 0x0B]);
        // SPI sends switches
        write_to_pipe(&spi_in_tx, vec![0x03, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response
        let data = read_from_pipe(&host_out_rx, 10);
        assert_eq!(data, [0x03, 0, 0, 0, 0, 0, 0, 0, 253, 0]);

        // Poll (SPI no longer dirty but node dirty)
        write_to_pipe(&host_in_tx, vec![0x00]);
        let data = read_from_pipe(&bus_out_rx, 1);
        assert_eq!(data, [0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x08]);
        // SPI sends switches (one changed)
        write_to_pipe(&spi_in_tx, vec![0x03, 0, 0, 0, 0, 0, 0, 0]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, [0x08]);

        {
            // Lock IOCTL mock to prevent other tests from breaking stuff
            let _guard = ioctl_custom::IOCTL_LOCK.lock().unwrap();
            *ioctl_custom::BACKLIGHT_BRIGHTNESS.lock().unwrap() = 0x12345678;

            // Set backlight via Spike 1/MPF command
            write_to_pipe(&host_in_tx, vec![0x80, 0x04, 0x80, 0x00, 0xff, 0xff, 0x00]);
            // Check that it got translated to spike 2 command
            let data = read_from_pipe(&bus_out_rx, 4);
            assert_eq!(data, [0x09, 0x02, 0xff, 0x00]);
            // Assert backlight has not been touched
            assert_eq!(*ioctl_custom::BACKLIGHT_BRIGHTNESS.lock().unwrap(), 0x12345678);

            // Set backlight via Spike 2 command
            write_to_pipe(&host_in_tx, vec![0x09, 0x02, 0xcc, 0x11, 0x00]);
            // Check that it got forwarded to the bus
            let data = read_from_pipe(&bus_out_rx, 4);
            assert_eq!(data, [0x09, 0x02, 0xcc, 0x11]);
            // Assert backlight has not been touched
            assert_eq!(*ioctl_custom::BACKLIGHT_BRIGHTNESS.lock().unwrap(), 0x12345678);
        }

        // SetResponseTime
        write_to_pipe(&host_in_tx, vec![0x06, 0x02, 0x45, 0x03, 0x00]);
        // Check that node got message
        let data = read_from_pipe(&bus_out_rx, 4);
        assert_eq!(data, [0x06, 0x02, 0x45, 0x03]);
        // No response

        // GetBridgeVersion
        write_to_pipe(&host_in_tx, vec![0x03, 0x00, 0x03]);
        // Check that node got message
        let data = read_from_pipe(&bus_out_rx, 2);
        assert_eq!(data, [0x03, 0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x01, 0x00, 0x03]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 3);
        assert_eq!(data, vec![0x01, 0x00, 0x03]);

        // GetBridgeState
        write_to_pipe(&host_in_tx, vec![0x05, 0x00, 0x01]);
        // Check that node got message
        let data = read_from_pipe(&bus_out_rx, 2);
        assert_eq!(data, [0x05, 0x00]);
        // Node sends response
        write_to_pipe(&bus_in_tx, vec![0x18]);
        // Check that host got the response from the node
        let data = read_from_pipe(&host_out_rx, 1);
        assert_eq!(data, vec![0x18]);

        // Quit
        write_to_pipe(&host_in_tx, vec![0xF5]);

        for thread in threads {
           match thread.join() {
               Ok(_) => {},
               Err(_) => {panic!("Thread crashed")},
           }
        }
    }
}


fn main() {
    println!("MPF Spike Bridge!");
    stderrlog::new().module(module_path!()).verbosity(10).init().unwrap();
    trace!("MPF Spike Bridge Started!");
    trace!("Git Hash: {}", env!("GIT_HASH"));

    // Parse args first
    let args: Vec<String> = env::args().collect();

    let spike_version;

    // First arg is serial speed
    let serial_speed;
    if args.len() >= 2 {
        match args[1].as_ref() {
            "230400" => {serial_speed = B230400},
            "460800" => {serial_speed = B460800},
            "576000" => {serial_speed = B576000},
            "921600" => {serial_speed = B921600},
            "1000000" => {serial_speed = B1000000},
            "1152000" => {serial_speed = B1152000},
            "1500000" => {serial_speed = B1500000},
            "2000000" => {serial_speed = B2000000},
            "2500000" => {serial_speed = B2500000},
            "3000000" => {serial_speed = B3000000},
            "3500000" => {serial_speed = B3500000},
            "4000000" => {serial_speed = B4000000},
            _ => {serial_speed = 0 as u32}
        }
    } else {
        serial_speed = 0 as u32;
    }
    if args.len() >= 3 {
        match args[2].as_ref() {
            "SPIKE1" => {spike_version = SpikeVersion::Spike1},
            "SPIKE2" => {spike_version = SpikeVersion::Spike2},
            _ => {panic!("Invalid spike version! {}", args[2]); }
        }
    } else {
        // Default to Spike 1
        spike_version = SpikeVersion::Spike1;
    }


    let bus_device;
    let spi_device;
    match spike_version {
        SpikeVersion::Spike1 => {
            trace!("Spike 1 early init");
            bus_device = "/dev/ttyS4";
            spi_device = "/dev/spi1";
        },
        SpikeVersion::Spike2 => {
            trace!("Spike 2 early init");
            // SetIspPin to true
            let mut isp_pin = gpio::sysfs::SysFsGpioOutput::open(75).unwrap();
            isp_pin.set_high().expect("Setting ISP failed.");

            bus_device = "/dev/ttymxc1";
            spi_device = "/dev/spidev1.0";
        },
    }

    let std_in;
    let std_out;
    unsafe {
        std_out = File::from_raw_fd(1);
        std_in = File::from_raw_fd(0);
    }

    // Switch baud rate
    let termios_old = Termios::from_fd(std_in.as_raw_fd()).unwrap();
    let mut termios = Termios::from_fd(std_in.as_raw_fd()).unwrap();
    cfmakeraw(&mut termios);

    if serial_speed > 0 {
        cfsetspeed(&mut termios, serial_speed).unwrap();
    }
    tcsetattr(std_in.as_raw_fd(), TCSAFLUSH, &termios).unwrap();

    let host_fd_in = TimeoutReader::new(std_in.try_clone().unwrap(), Duration::new(5, 0));
    let host_fd_out = std_out;

    // Open the bus
    let bus_fd = OpenOptions::new().write(true).read(true).custom_flags(libc::O_SYNC | libc::O_NOCTTY).open(bus_device).unwrap();

    let mut termios = Termios::from_fd(bus_fd.as_raw_fd()).unwrap();
    cfmakeraw(&mut termios);
    cfsetspeed(&mut termios, B460800).unwrap();

    /* fetch bytes as they become available */
//    termios.c_cc[VMIN] = 1;
//    termios.c_cc[VTIME] = 1;
    tcsetattr(bus_fd.as_raw_fd(), TCSANOW, &termios).unwrap();

    let bus_fd2 = bus_fd.try_clone().unwrap();
    let mut bus_init = bus_fd.try_clone().unwrap();
    let bus_fd_raw = bus_fd.as_raw_fd();
    let bus_fd_reader = TimeoutReader::new(bus_fd.try_clone().unwrap(), Duration::from_millis(200));

    // Open SPI for local switches
    let spi_fd = OpenOptions::new().write(true).read(true).custom_flags(libc::O_SYNC | libc::O_NOCTTY).open(spi_device).unwrap();

    // Open DMD and backlight (Spike 1 only)
    let dmd_fd;
    let backlight_fd;
    match &spike_version {
        SpikeVersion::Spike1 => {
            trace!("Spike 1 late init");
            dmd_fd = Some(OpenOptions::new().write(true).read(true).custom_flags(libc::O_SYNC | libc::O_NOCTTY).open("/dev/spi0").unwrap());
            backlight_fd = Some(File::open("/dev/backlight").unwrap());

            // Open magic gpio device
            let gpio_fd = File::open("/dev/gpio").unwrap();

            unsafe {
                // From NODEBUS_Init
                ioctl_custom::set_gpio_on(gpio_fd.as_raw_fd(), 0x8C).unwrap();
                ioctl_custom::set_gpio_off(gpio_fd.as_raw_fd(), 0x8A).unwrap();

                // Enable nodebus power
                ioctl_custom::set_gpio_on(gpio_fd.as_raw_fd(), 0x6B).unwrap();
                // Enable amp (disabled for now because we do not use it anyway)
                //ioctl_custom::set_gpio_on(gpio_fd.as_raw_fd(), 0x6A).unwrap();

                // From NODEBUS_Init (only during open)
                ioctl_custom::set_gpio_on(gpio_fd.as_raw_fd(), 0x8E).unwrap();
                thread::sleep(Duration::from_millis(5 as u64));
                ioctl_custom::set_gpio_off(gpio_fd.as_raw_fd(), 0x8E).unwrap();
                thread::sleep(Duration::from_millis(5 as u64));

                // Disable nodebus power
                ioctl_custom::set_gpio_off(gpio_fd.as_raw_fd(), 0x6B).unwrap();
                thread::sleep(Duration::from_millis(1000 as u64));

                // Enable nodebus power again
                ioctl_custom::set_gpio_on(gpio_fd.as_raw_fd(), 0x6B).unwrap();
                thread::sleep(Duration::from_millis(500 as u64));
            }
        },
        SpikeVersion::Spike2 => {
            trace!("Spike 2 late init");
            dmd_fd = None;
            backlight_fd = None;

            // This might reset the netbridge CPU
            ioctl::tiocmbis(bus_fd.as_raw_fd(), ioctl::TIOCM_RTS as c_int).expect("Setting RTS failed.");
            thread::sleep(Duration::from_millis(5 as u64));
            ioctl::tiocmbic(bus_fd.as_raw_fd(), ioctl::TIOCM_RTS as c_int).expect("Clearing RTS failed.");
            thread::sleep(Duration::from_millis(5 as u64));

            // SetPower on
            bus_init.write(vec![0x07, 0x01, 0x01].as_slice()).unwrap();
            bus_init.flush().unwrap();

            // SetPower off
            bus_init.write(vec![0x07, 0x01, 0x00].as_slice()).unwrap();
            bus_init.flush().unwrap();

            thread::sleep(Duration::from_millis(1000 as u64));

            // SetPower on again
            bus_init.write(vec![0x07, 0x01, 0x01].as_slice()).unwrap();
            bus_init.flush().unwrap();

            thread::sleep(Duration::from_millis(500 as u64));
        },
    }

    trace!("Starting threads!");
    let threads = run_threads(host_fd_in, host_fd_out, bus_fd_reader, bus_fd_raw, bus_fd2, spi_fd, dmd_fd, backlight_fd, spike_version.clone());

    trace!("Waiting for threads!");
    for thread in threads {
        thread.join().unwrap();
    }
    trace!("All threads stopped.");

    match &spike_version {
        SpikeVersion::Spike1 => {
            // Open magic gpio device
            let gpio_fd = File::open("/dev/gpio").unwrap();
            unsafe {
                // Disable nodebus power
                ioctl_custom::set_gpio_off(gpio_fd.as_raw_fd(), 0x6B).unwrap();
                // Disable amp
                ioctl_custom::set_gpio_off(gpio_fd.as_raw_fd(), 0x6A).unwrap();
            }
        },
        SpikeVersion::Spike2 => {
            // Reset Bus
            bus_init.write(vec![0x80, 0x01, 0xF1].as_slice()).unwrap();
            // SetPower off - this is safe because all threads stopped
            bus_init.write(vec![0x07, 0x01, 0x00].as_slice()).unwrap();
            bus_init.flush().unwrap();
        },
    }
    tcsetattr(std_in.as_raw_fd(), TCSAFLUSH, &termios_old).unwrap();
    println!("Resetting terminal mode and quitting.");
    info!("Quitting");
}


fn host_thread<HIn: std::io::Read + std::marker::Send>(host_fd_in: &mut HIn, bus_tx: &Sender<NodeBusMessage>, spi_tx: &Sender<SpiMessage>, spike_version: &SpikeVersion) -> Result<bool, Error> {
    // First read node byte
    let mut node = [0; 1];
    trace!("Host: Reading Node Info.");
    host_fd_in.read_exact(&mut node)?;
    match node[0] {
        0 => {
            trace!("Host: Got Poll");
            // Poll node bus
            let message = NodeBusMessage::Poll {};
            match bus_tx.send(message) {
                Ok(_) => {},
                Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted, "Could not send poll to nodebus"));},
            }
            // Poll SPI
            let message = SpiMessage::Poll {};
            match spi_tx.send(message) {
                Ok(_) => {},
                Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted, "Could not send poll to SPI"));},
            }
            Ok(false)
        },
        1 => {
            trace!("Host: Got sleep");
            // Sleep in node bus
            let mut wait_ms = [0; 1];
            host_fd_in.read_exact(&mut wait_ms)?;
            let message = NodeBusMessage::Wait { wait_ms: wait_ms[0] };
            trace!("Host: Got sleep for {}", wait_ms[0]);
            match bus_tx.send(message) {
                Ok(_) => {},
                Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted, "Could not send sleep to nodebus"));},
            }
            Ok(false)
        },
        2 ..= 10 => {
            trace!("Host: Got Bridge Command");
            let mut len = [0; 1];
            host_fd_in.read_exact(&mut len)?;
            let mut data: [u8; 256] = [0; 256];
            if len[0] > 0 {
                host_fd_in.read_exact(&mut data[0..(len[0] as usize)])?;
            }
            let mut response_len = [0; 1];
            host_fd_in.read_exact(&mut response_len)?;

            if node[0] == 0x09 {
                if let SpikeVersion::Spike1{} = spike_version {
                    // LEDs on local node/Backlight
                    trace!("Host: Set backlight. Intercepting Spike 2 command for Spike 1.");
                    let message = SpiMessage::BacklightLed {
                        brightness: data[0]
                    };
                    match spi_tx.send(message) {
                        Ok(_) => {},
                        Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted,
                                                         "Could not send set backlight to spi"));},
                    }
                    return Ok(false)
                }
            }

            let message = NodeBusMessage::BridgeMessage {
                cmd: node[0],
                len: len[0],
                data,
                response_len: response_len[0]
            };
            match bus_tx.send(message) {
                Ok(_) => {},
                Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted, "Could not send bridge command to nodebus"));},
            }
            Ok(false)
        }
        0xF5 => {
            // Exit
            trace!("Host: Got exit");
            Ok(true)
        },
        _ => {
            // Commands to nodes
            let mut len = [0; 1];
            host_fd_in.read_exact(&mut len)?;
            let mut cmd = [0; 1];
            host_fd_in.read_exact(&mut cmd)?;
            if node[0] == 0x80 && len[0] == 0 && cmd[0] == 0x90 {
                // DMD frame
                trace!("Host: Got DMD frame");
                let mut data: [u8; 2048] = [0; 2048];
                host_fd_in.read_exact(&mut data)?;
                let message = SpiMessage::DmdFrame {
                    data
                };
                trace!("Host: Got DMD frame data");
                match spi_tx.send(message) {
                    Ok(_) => {},
                    Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted, "Could not send DMD frame"));},
                }
                Ok(false)
            } else {
                trace!("Host: Got node command");
                let data_len: usize = (len[0] - 2) as usize;
                let mut data: [u8; 256] = [0; 256];
                host_fd_in.read_exact(&mut data[0..(data_len as usize)])?;
                let mut checksum = [0; 1];
                host_fd_in.read_exact(&mut checksum)?;
                let mut response_len = [0; 1];
                host_fd_in.read_exact(&mut response_len)?;
                if node[0] == 0x80 && cmd[0] == 0x11 {
                    // Read local switches
                    trace!("Host: Read local switches");
                    let message = SpiMessage::ReadLocalSwitches {};
                    match spi_tx.send(message) {
                        Ok(_) => {},
                        Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted, "Could not send read switches to spi"));},
                    }
                    Ok(false)
                } else if node[0] == 0x80 && len[0] == 4 && cmd[0] == 0x80 {
                    // LEDs on local node/Backlight
                    match spike_version {
                        SpikeVersion::Spike1{} => {
                            trace!("Host: Set backlight via LED command.");
                            let message = SpiMessage::BacklightLed {
                                brightness: data[1]
                            };
                            match spi_tx.send(message) {
                                Ok(_) => {},
                                Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted, "Could not send set backlight to spi"));},
                            }
                            Ok(false)
                        },
                        SpikeVersion::Spike2{} => {
                            trace!("Host: Set backlight via LED command. Emulate Spike 1 command for Spike 2.");
                            let mut bridge_message: [u8; 256] = [0; 256];
                            bridge_message[0] = data[1];
                            let message = NodeBusMessage::BridgeMessage {
                                cmd: 0x09,
                                len: 2,
                                data: bridge_message,
                                response_len: 0
                            };
                            match bus_tx.send(message) {
                                Ok(_) => {},
                                Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted, "Could not send bridge command to nodebus"));},
                            }
                            Ok(false)
                        }
                    }
                } else {
                    // Forward to node bus
                    trace!("Host: Got nodebus message");
                    let message = NodeBusMessage::BusMessage {
                        node: node[0],
                        len: len[0],
                        cmd: cmd[0],
                        data,
                        checksum: checksum[0],
                        response_len: response_len[0]
                    };
                    match bus_tx.send(message) {
                        Ok(_) => {},
                        Err(_) => {return Err(Error::new(ErrorKind::ConnectionAborted, "Could not send message to nodebus"));},
                    }
                    Ok(false)
                }
            }
        }
    }
}


fn run_threads<HIn: std::io::Read + std::marker::Send + 'static, HOut: std::io::Write + std::marker::Send + 'static,
    BIn: std::io::Read + std::marker::Send + 'static,
    BOut: std::io::Write + std::marker::Send + std::os::unix::io::AsRawFd + 'static,
    SIn: std::io::Read + std::marker::Send + 'static, SOut: std::io::Write + std::marker::Send + 'static,
    Bl: std::marker::Send + std::os::unix::io::AsRawFd + 'static>
(host_fd_in: HIn, host_fd_out: HOut, bus_fd_in: BIn,  bus_fd_in_raw: i32, bus_fd_out: BOut, spi_fd_in: SIn,
 dmd_fd: Option<SOut>, backlight_fd: Option<Bl>, spike_version: SpikeVersion) -> Vec<JoinHandle<()>> {
    // Talk to the spike bus
    let (bus_tx, bus_rx) = mpsc::channel::<NodeBusMessage>();

    // Talk to the host/MPF
    let (host_tx, host_rx) = mpsc::channel::<Response>();
    let host_tx2 = host_tx.clone();

    // Talk to SPI/local stuff
    let (spi_tx, spi_rx) = mpsc::channel::<SpiMessage>();

    let bus_handler = thread::spawn(move || {
        let mut fd_in = bus_fd_in;
        let mut fd_out = bus_fd_out;
        loop {
            trace!("Bus: Waiting for message");
            let received = bus_rx.recv();

            // Check if we got stray data in between
            match fionread(bus_fd_in_raw) {
                Ok(bytes) => {
                    if bytes > 0 {
                        let mut unexpected_data = [0; 256];
                        warn!("Bus: Got {} unexpected bytes in on bus.", bytes);
                        match fd_in.read_exact(&mut unexpected_data[0..(bytes as usize)]) {
                            Ok(_) => {
                                trace!("Bus: Got unexpected data: {:x?}", &unexpected_data[0..(bytes as usize)]);
                            },
                            Err(err) => {
                                error!("Bus: Got error during read of unexpected bytes: {}", err);
                            },
                        }
                    }
                },
                Err(e) => {warn!("Bus: Failed to get bytes from bus: {}", e);}
            }

            // flush input
            unsafe {
                libc::tcflush(bus_fd_in_raw, TCIFLUSH);
            }
            trace!("Bus: Flushed bus_fd_in");
            match received {
                Ok(message) => {
                    match message {
                        NodeBusMessage::BusMessage { node, cmd, len, data, checksum, response_len } => {
                            let mut message = vec!();
                            message.extend(&[node, len, cmd]);
                            message.extend(&data[0..((len - 2) as usize)]);
                            message.extend(&[checksum, response_len]);
                            fd_out.write(message.as_slice()).unwrap();
                            fd_out.flush().unwrap();
                            trace!("Bus: Sending message. node: {} cmd: {} len: {} response_len: {} msg: {:x?}", node & 0x3f, cmd, len, response_len, &message);
                            if response_len > 0 {
                                trace!("Bus: Reading input");
                                let mut response = [0; 256];
                                match fd_in.read_exact(&mut response[0..(response_len as usize)]) {
                                    Ok(_) => {
                                        trace!("Bus: Got response: {:x?}", &response[0..(response_len as usize)]);
                                        // Forward response to host
                                        match host_tx.send(Response::Message { data: response, len: response_len }) {
                                            Ok(_) => {},
                                            Err(err) => {error!("Bus: Got error sending to host {}", err); return;},
                                        }
                                    },
                                    Err(err) => {
                                        error!("Bus: Got error during read: {}", err);
                                        // Send desync response with corrent length
                                        match host_tx.send(Response::Message { data: [55; 256], len: response_len }) {
                                            Ok(_) => {},
                                            Err(err) => {error!("Bus: Got error sending to host {}", err); return;},
                                        }
                                        // Recover the bus
                                        unsafe {
                                            libc::tcsendbreak(fd_out.as_raw_fd(), 0);
                                        }
                                    },
                                }
                            }
                        },
                        NodeBusMessage::BridgeMessage { cmd, len, data, response_len } => {
                            let mut message = vec!();
                            message.extend(&[cmd, len]);
                            if len > 0 {
                                message.extend(&data[0..(len as usize)]);
                            }
                            fd_out.write(message.as_slice()).unwrap();
                            fd_out.flush().unwrap();
                            trace!("Bus: Sending message to bridge. cmd: {} len: {} response_len: {} msg: {:x?}", cmd, len, response_len, &message);
                            if response_len > 0 {
                                trace!("Bus: Reading input");
                                let mut response = [0; 256];
                                match fd_in.read_exact(&mut response[0..(response_len as usize)]) {
                                    Ok(_) => {
                                        trace!("Bus: Got response");
                                        // Forward response to host
                                        match host_tx.send(Response::Message { data: response, len: response_len }) {
                                            Ok(_) => {},
                                            Err(err) => {error!("Bus: Got error sending to host {}", err); return;},
                                        }
                                    },
                                    Err(err) => {
                                        error!("Bus: Got error during read: {}", err);
                                        // Send desync response with corrent length
                                        match host_tx.send(Response::Message { data: [54; 256], len: response_len }) {
                                            Ok(_) => {},
                                            Err(err) => {error!("Bus: Got error sending to host {}", err); return;},
                                        }
                                        // Recover the bus
                                        unsafe {
                                            libc::tcsendbreak(fd_out.as_raw_fd(), 0);
                                        }
                                    },
                                }
                            }

                            // Check if we got additional data after command (even without any readback)
                            match fionread(bus_fd_in_raw) {
                                Ok(bytes) => {
                                    if bytes > 0 {
                                        warn!("Bus: Got {} unexpected bytes in on bus after command {}.", bytes, cmd);
                                        let mut unexpected_data = [0; 256];
                                        match fd_in.read_exact(&mut unexpected_data[0..(bytes as usize)]) {
                                            Ok(_) => {
                                                trace!("Bus: Got unexpected data: {:x?}", &unexpected_data[0..(bytes as usize)]);
                                            },
                                            Err(err) => {
                                                error!("Bus: Got error during read of unexpected bytes: {}", err);
                                            },
                                        }

                                        // flush input
                                        unsafe {
                                            libc::tcflush(bus_fd_in_raw, TCIFLUSH);
                                        }
                                    }
                                },
                                Err(e) => {warn!("Bus: Failed to get bytes from bus: {}", e);}
                            }
                        },
                        NodeBusMessage::Poll => {
                            trace!("Bus: Poll");
                            fd_out.write(&[0x00]).unwrap();
                            fd_out.flush().unwrap();
                            let mut response = [0; 1];
                            let message;
                            match fd_in.read_exact(&mut response) {
                                Ok(_) => {
                                    message = Response::Poll { result: response[0] }

                                },
                                Err(err) => {
                                    error!("Bus: Got error {}", err);
                                    message = Response::Poll { result: 55 };
                                },
                            }
                            match host_tx.send(message) {
                                Ok(_) => {},
                                Err(err) => {error!("Bus: Got error sending to host {}", err); return;},
                            }
                        },
                        NodeBusMessage::Wait { wait_ms } => {
                            trace!("Bus: Sleep {}", wait_ms);
                            thread::sleep(Duration::from_millis(wait_ms as u64));
                        },
                        NodeBusMessage::Exit => {
                            trace!("Bus: Got exit");
                            let _result = host_tx.send(Response::Exit {});
                            // result is intentionally ignored since we exit anyway
                            return;
                        },
                    }
                }
                Err(err) => {
                    error!("Bus: Got error during message receiving: {}", err);
                    return;
                }
            }
        }
    });

    let host_stdin_handler = thread::spawn(move || {
        let mut host_fd_in = host_fd_in;
        loop {
            match host_thread(&mut host_fd_in, &bus_tx, &spi_tx, &spike_version) {
                Ok(done) => {if done {info!("Host: Thread done"); break;}},
                Err(err) => {error!("Host: Got error {}", err); break;},
            }
        }
        let _result = bus_tx.send(NodeBusMessage::Exit {});
        // Result is unused because we exit
        let _result = spi_tx.send(SpiMessage::Exit {});
        // Result is unused because we exit

    });

    let host_stdout_handler = thread::spawn(move || {
        let mut fd = host_fd_out;
        let mut poll_result_spi = PollResult::Unknown{};
        let mut poll_result_node = PollResult::Unknown{};
        loop {
            trace!("Stdout: Waiting for message");
            let received = host_rx.recv();
            match received {
                Ok(response) => {
                    match response {
                        Response::SpiPoll { dirty } => {
                            trace!("Stdout: Got SPI poll result");
                            if dirty {
                                poll_result_spi = PollResult::Dirty {
                                    result: 0xF0
                                };
                            } else {
                                poll_result_spi = PollResult::Clean {};
                            }
                        },
                        Response::Message { data, len } => {
                            trace!("Stdout: Got raw message");
                            fd.write(&data[0..(len as usize)]).unwrap();
                            fd.flush().unwrap();
                        },
                        Response::Poll { result } => {
                            trace!("Stdout: Got Nodebus poll result");
                            if result > 0 {
                                poll_result_node = PollResult::Dirty {
                                    result
                                };
                            } else {
                                poll_result_node = PollResult::Clean {};
                            }
                        },
                        Response::Exit => {trace!("Stdout: Got exit"); return },
                    }
                    match (&poll_result_spi, &poll_result_node) {
                        (PollResult::Unknown, _) => {},
                        (_, PollResult::Unknown) => {},
                        (PollResult::Dirty { .. }, _) => {
                            trace!("Stdout: Sending SPI dirty");
                            fd.write(&[0xF0]).unwrap();
                            fd.flush().unwrap();
                            poll_result_spi = PollResult::Unknown {};
                            poll_result_node = PollResult::Unknown {};
                        },
                        (_, PollResult::Dirty { result }) => {
                            trace!("Stdout: Sending nodebus dirty");
                            fd.write(&[*result]).unwrap();
                            fd.flush().unwrap();
                            poll_result_spi = PollResult::Unknown {};
                            poll_result_node = PollResult::Unknown {};
                        },
                        (_, PollResult::Clean) => {
                            trace!("Stdout: Sending poll clean");
                            fd.write(&[0]).unwrap();
                            fd.flush().unwrap();
                            poll_result_spi = PollResult::Unknown {};
                            poll_result_node = PollResult::Unknown {};
                        },
                    }
                },
                Err(err) => {
                    error!("Stdout: Got error: {}", err);
                    return;
                }
            }
        }
    });

    let spi_handler = thread::spawn(move || {
        let mut last_switch_state: [u8; 8] = [0; 8];
        let mut fd_in = spi_fd_in;
        let mut fd_out = dmd_fd;
        loop {
            trace!("SPI: Waiting for message");
            let response = spi_rx.recv();
            match response {
                Ok(message) => {
                    match message {
                        SpiMessage::Poll {} => {
                            trace!("SPI: Got poll");
                            // Read SPI here
                            let mut switch_state = [0; 8];
                            fd_in.read_exact(&mut switch_state).unwrap();
                            match host_tx2.send(Response::SpiPoll { dirty: switch_state != last_switch_state }) {
                                Ok(_) => {},
                                Err(err) => {error!("SPI: Error sending to host (1): {}", err); return;}
                            }

                        },
                        SpiMessage::DmdFrame { data } => {
                            // Write to DMD via SPI
                            match fd_out.as_mut() {
                                None => {
                                    warn!("Got DMD frame without DMD device.")
                                },
                                Some(dmd_fd) => {
                                    trace!("SPI: Writing DMD frame");
                                    dmd_fd.write(&data).unwrap();
                                },
                            }
                        },
                        SpiMessage::BacklightLed { brightness } => {
                            match backlight_fd.as_ref() {
                                None => {
                                    warn!("Tried to set backlight without device.")
                                },
                                Some(fd) => {
                                    // Control backlight LED
                                    trace!("SPI: Setting backlight to {}", brightness);
                                    let brightness = brightness as libc::c_int * 256;
                                    unsafe {
                                        ioctl_custom::set_brightness(fd.as_raw_fd(), &brightness).unwrap();
                                    }
                                },
                            }
                        },
                        SpiMessage::ReadLocalSwitches => {
                            trace!("SPI: Reading local switches");
                            // Read local switches
                            let mut switch_state = [0; 8];
                            fd_in.read_exact(&mut switch_state).unwrap();
                            let mut checksum: u32 = 0;
                            last_switch_state = switch_state;
                            for switch in switch_state.iter() {
                                checksum += *switch as u32;
                            }
                            let mut data: [u8; 256] = [0; 256];
                            data[0..8].clone_from_slice(&switch_state);
                            data[8..10].clone_from_slice(&[256_u16.wrapping_sub((checksum & 0xFF) as u16) as u8, 0]);
                            match host_tx2.send(Response::Message { data, len: 10 }) {
                                Ok(_) => {},
                                Err(err) => {error!("SPI: Error sending to host (2): {}", err); return;}
                            }
                        },
                        SpiMessage::Exit => {
                            trace!("SPI: Got exit");
                            return;
                        },
                    }
                },
                Err(err) => {
                    error!("SPI: Error reading message: {}", err);
                    return;
                }
            }
        }
    });

    return vec![bus_handler, host_stdin_handler, host_stdout_handler, spi_handler];
}

