//! Windows: ACPI thermal zones through the **PDH performance counter**
//! `\Thermal Zone Information(*)\High Precision Temperature` (tenths of K),
//! or `...\Temperature` (K) on older builds. Unlike WMI
//! `MSAcpi_ThermalZoneTemperature` it needs **no admin rights**, and no
//! driver (LibreHardwareMonitor's WinRing0 is blocked on many work laptops).
//!
//! Caveat: these are firmware-defined zones, not the CPU die. On most laptops
//! they track the CPU/EC temperature. On some they're missing or stuck at a
//! constant. The caller wraps this in a stuck-value guard that falls back to
//! the load estimate.

use anyhow::{Result, bail};
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW,
};
use windows::core::{HSTRING, PCWSTR};

use super::{Reading, Source};

const COUNTERS: &[(&str, f64)] = &[
    (
        r"\Thermal Zone Information(*)\High Precision Temperature",
        10.0,
    ),
    (r"\Thermal Zone Information(*)\Temperature", 1.0),
];

pub struct ThermalZones {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    path: &'static str,
    /// Divisor to get Kelvin (10 for "High Precision").
    scale: f64,
    /// Reused between reads; u64 storage keeps the item array aligned.
    buf: Vec<u64>,
}

// The PDH handles are only used from the one task that owns this.
unsafe impl Send for ThermalZones {}

impl Drop for ThermalZones {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

pub fn detect() -> Option<ThermalZones> {
    for &(path, scale) in COUNTERS {
        let mut query = PDH_HQUERY::default();
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 {
            return None;
        }
        let mut counter = PDH_HCOUNTER::default();
        let ok =
            unsafe { PdhAddEnglishCounterW(query, &HSTRING::from(path), 0, &mut counter) } == 0;
        if ok {
            let mut z = ThermalZones {
                query,
                counter,
                path,
                scale,
                buf: Vec::new(),
            };
            if z.read().is_ok() {
                return Some(z);
            }
            // drop closes the query
        } else {
            unsafe { PdhCloseQuery(query) };
        }
    }
    None
}

impl ThermalZones {
    fn max_kelvin(&mut self) -> Result<f64> {
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            bail!("PdhCollectQueryData failed");
        }
        let (mut size, mut count) = (0u32, 0u32);
        let r = unsafe {
            PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None)
        };
        if r != PDH_MORE_DATA || size == 0 {
            bail!("no thermal zones (PDH 0x{r:08x})");
        }
        let words = (size as usize).div_ceil(8);
        if self.buf.len() < words {
            self.buf.resize(words, 0);
        }
        let items = self.buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let r = unsafe {
            PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(items),
            )
        };
        if r != 0 {
            bail!("PdhGetFormattedCounterArrayW failed (0x{r:08x})");
        }
        let items = unsafe { std::slice::from_raw_parts(items, count as usize) };
        let max = items
            .iter()
            .filter(|i| i.FmtValue.CStatus <= 1) // PDH_CSTATUS_VALID_DATA / NEW_DATA
            .map(|i| unsafe { i.FmtValue.Anonymous.doubleValue } / self.scale)
            .fold(f64::NAN, f64::max);
        if max.is_nan() {
            bail!("no valid thermal zone values");
        }
        Ok(max)
    }
}

impl Source for ThermalZones {
    fn describe(&self) -> String {
        format!("Windows ACPI thermal zones, hottest ({})", self.path)
    }

    fn read(&mut self) -> Result<Reading> {
        let c = self.max_kelvin()? - 273.15;
        // Below ~15 °C or above 110 °C is a placeholder or garbage, not a CPU.
        if !(15.0..=110.0).contains(&c) {
            bail!("implausible thermal zone value {c:.1} °C");
        }
        Ok(Reading {
            celsius: c as f32,
            estimated: false,
        })
    }
}
