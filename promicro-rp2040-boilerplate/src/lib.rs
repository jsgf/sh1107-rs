#![no_std]
#![feature(type_alias_impl_trait)]
#![feature(generic_associated_types)]

use core::{
    cell::RefCell,
    ops::{Deref, DerefMut},
    task::{Poll, Waker},
};

use critical_section::Mutex;
use embedded_hal_async::i2c::ErrorType;
use fugit::{MicrosDurationU32, RateExtU32};
use futures::{future, Future};
use panic_probe as _;
use sparkfun_pro_micro_rp2040::{hal, Pins};

use hal::{
    gpio::{bank0, FunctionI2C, Pin},
    pac::{self, interrupt},
    sio::Sio,
    timer::{Alarm, Alarm0},
    watchdog::Watchdog,
    Clock,
};

use rp2040_async_i2c::AsyncI2C;

pub use embedded_hal_async::i2c::SevenBitAddress;
pub use hal::timer::Timer;
pub use sparkfun_pro_micro_rp2040::entry;

type I2CPeriphInner = AsyncI2C<
    pac::I2C0,
    (
        Pin<bank0::Gpio16, FunctionI2C>,
        Pin<bank0::Gpio17, FunctionI2C>,
    ),
>;

pub struct I2CPeriph(I2CPeriphInner);
impl Deref for I2CPeriph {
    type Target = I2CPeriphInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for I2CPeriph {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl ErrorType for I2CPeriph {
    type Error = <I2CPeriphInner as ErrorType>::Error;
}
impl sh1107::WriteIter<SevenBitAddress> for I2CPeriph {
    type WriteIterFuture<'a, U>
    = impl Future<Output = Result<(), Self::Error>> + 'a
    where
        Self: 'a,
        U: 'a;

    fn write_iter<'a, U>(
        &'a mut self,
        address: SevenBitAddress,
        bytes: U,
    ) -> Self::WriteIterFuture<'a, U>
    where
        U: IntoIterator<Item = u8> + 'a,
    {
        self.0.write_iter(address, bytes)
    }
}

type Alarm0WakerCTX = (Alarm0, Option<Waker>);
static ALARM0_WAKER: Mutex<RefCell<Option<Alarm0WakerCTX>>> = Mutex::new(RefCell::new(None));
pub async fn wait_for(timer: &Timer, delay: u32) {
    if delay < 20 {
        let start = timer.get_counter_low();
        future::poll_fn(|cx| {
            cx.waker().wake_by_ref();
            if timer.get_counter_low().wrapping_sub(start) < delay {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
    } else {
        let mut started = false;
        future::poll_fn(move |cx| {
            critical_section::with(|cs| {
                if let Some((alarm, waker)) = ALARM0_WAKER.borrow_ref_mut(cs).as_mut() {
                    if !started {
                        alarm.clear_interrupt();
                        alarm.enable_interrupt();
                        alarm.schedule(MicrosDurationU32::micros(delay)).unwrap();
                        started = true;
                        *waker = Some(cx.waker().clone());
                        Poll::Pending
                    } else if alarm.finished() {
                        Poll::Ready(())
                    } else {
                        *waker = Some(cx.waker().clone());
                        Poll::Pending
                    }
                } else {
                    unreachable!()
                }
            })
        })
        .await;
    }
}

pub async fn timed<T>(op: &str, timer: &Timer, fut: impl Future<Output = T>) -> T {
    let start = timer.get_counter_low();
    let res = fut.await;
    let diff = timer.get_counter_low().wrapping_sub(start);
    defmt::info!("{} took {}us", op, diff);
    res
}

#[interrupt]
#[allow(non_snake_case)]
fn TIMER_IRQ_0() {
    critical_section::with(|cs| {
        ALARM0_WAKER
            .borrow_ref_mut(cs)
            .as_mut()
            .and_then(|(alarm, waker)| {
                alarm.disable_interrupt();
                waker.take()
            })
            .map(|waker| waker.wake())
    });
}

static I2C_WAKER: Mutex<RefCell<Option<Waker>>> = Mutex::new(RefCell::new(None));
pub fn waker_setter(waker: Waker) {
    critical_section::with(|cs| {
        *I2C_WAKER.borrow_ref_mut(cs) = Some(waker);
    });
}

#[interrupt]
#[allow(non_snake_case)]
fn I2C0_IRQ() {
    critical_section::with(|cs| {
        let i2c0 = unsafe { &pac::Peripherals::steal().I2C0 };
        i2c0.ic_intr_mask.modify(|_, w| {
            w.m_tx_empty()
                .enabled()
                .m_rx_full()
                .enabled()
                .m_tx_abrt()
                .enabled()
        });
        I2C_WAKER
            .borrow_ref_mut(cs)
            .take()
            .map(|waker| waker.wake())
    });
}

pub fn init() -> (Timer, I2CPeriph) {
    let mut pac = pac::Peripherals::take().unwrap();
    let _core = pac::CorePeripherals::take().unwrap();

    let mut watchdog = Watchdog::new(pac.WATCHDOG);
    let clocks = hal::clocks::init_clocks_and_plls(
        sparkfun_pro_micro_rp2040::XOSC_CRYSTAL_FREQ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    let mut timer = Timer::new(pac.TIMER, &mut pac.RESETS);
    let alarm = timer.alarm_0().unwrap();

    let sio = Sio::new(pac.SIO);
    let pins = Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    let mut i2c_ctrl = AsyncI2C::new(
        pac.I2C0,
        pins.sda.into_mode(),
        pins.scl.into_mode(),
        400_000.Hz(),
        &mut pac.RESETS,
        clocks.system_clock.freq(),
    );
    i2c_ctrl.set_waker_setter(waker_setter);

    critical_section::with(move |cs| unsafe {
        pac::NVIC::unpend(pac::Interrupt::I2C0_IRQ);
        pac::NVIC::unmask(pac::Interrupt::I2C0_IRQ);
        pac::NVIC::unpend(pac::Interrupt::TIMER_IRQ_0);
        pac::NVIC::unmask(pac::Interrupt::TIMER_IRQ_0);
        *ALARM0_WAKER.borrow_ref_mut(cs) = Some((alarm, None));
    });

    (timer, I2CPeriph(i2c_ctrl))
}
