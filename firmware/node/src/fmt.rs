//! Logging that goes to defmt (Pico) or `log` (ESP32), or nowhere. Format strings must
//! work for both, so only `{}` with integers and `&str` are used.

#![allow(unused_macros)]

macro_rules! info {
    ($s:literal $(, $x:expr)* $(,)?) => {{
        #[cfg(feature = "defmt")]
        ::defmt::info!($s $(, $x)*);
        #[cfg(all(feature = "log", not(feature = "defmt")))]
        ::log::info!($s $(, $x)*);
        #[cfg(not(any(feature = "defmt", feature = "log")))]
        let _ = ($( & $x ),*);
    }};
}

macro_rules! warn {
    ($s:literal $(, $x:expr)* $(,)?) => {{
        #[cfg(feature = "defmt")]
        ::defmt::warn!($s $(, $x)*);
        #[cfg(all(feature = "log", not(feature = "defmt")))]
        ::log::warn!($s $(, $x)*);
        #[cfg(not(any(feature = "defmt", feature = "log")))]
        let _ = ($( & $x ),*);
    }};
}
