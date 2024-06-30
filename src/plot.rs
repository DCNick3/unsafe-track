use crate::analysis::CommitResult;
use cargo_geiger_serde::Count;
use chrono::{NaiveDateTime, TimeDelta};
use clap::ValueEnum;
use plotters::coord::ranged1d::ValueFormatter;
use plotters::coord::types::RangedCoordu32;
use plotters::coord::Shift;
use plotters::prelude::*;
use serde::Deserialize;
use std::ops::Add;

fn convert_date(date: gix_date::Time) -> NaiveDateTime {
    NaiveDateTime::UNIX_EPOCH.add(TimeDelta::seconds(date.seconds))
}

trait XCoordTrait {
    type Ranged: Ranged<ValueType = Self::Value> + ValueFormatter<Self::Value>;
    type Value: Copy + Ord + 'static;

    const AXIS_DESCRIPTION: &'static str;

    fn get_x_coord(commit: &CommitResult) -> Self::Value;
    fn make_ranged(min: Self::Value, max: Self::Value) -> Self::Ranged;
}

pub struct XIsDate;
pub struct XIsIndex;

impl XCoordTrait for XIsDate {
    type Ranged = RangedDateTime<NaiveDateTime>;
    type Value = NaiveDateTime;

    const AXIS_DESCRIPTION: &'static str = "Date";

    fn get_x_coord(commit: &CommitResult) -> Self::Value {
        convert_date(commit.date)
    }

    fn make_ranged(min: Self::Value, max: Self::Value) -> Self::Ranged {
        RangedDateTime::from(min..max)
    }
}

impl XCoordTrait for XIsIndex {
    type Ranged = RangedCoordu32;
    type Value = u32;

    const AXIS_DESCRIPTION: &'static str = "Commit Index";

    fn get_x_coord(commit: &CommitResult) -> Self::Value {
        commit.index
    }

    fn make_ranged(min: Self::Value, max: Self::Value) -> Self::Ranged {
        RangedCoordu32::from(min..max)
    }
}

#[derive(Copy, Clone, Default, Debug, Deserialize, ValueEnum)]
pub enum XCoord {
    #[default]
    Index,
    Date,
}

#[derive(Copy, Clone, Default, Debug, Deserialize, ValueEnum)]
pub enum YCoord {
    #[default]
    Functions,
    Expressions,
}

impl YCoord {
    pub fn get_counts(&self, commit: &CommitResult) -> Count {
        match self {
            YCoord::Functions => {
                commit.counters.functions.clone() + commit.counters.methods.clone()
            }
            YCoord::Expressions => commit.counters.exprs.clone(),
        }
    }
}

pub fn plot_results_svg(results: &[CommitResult], x_coord: XCoord, y_coord: YCoord) -> String {
    let mut buf = String::new();
    let root = SVGBackend::with_string(&mut buf, (1280, 600)).into_drawing_area();
    plot_results(results, x_coord, y_coord, &root);
    drop(root);
    buf
}

#[tracing::instrument(skip(results, root), fields(result_count = results.len()))]
pub fn plot_results<DB>(
    results: &[CommitResult],
    x_coord: XCoord,
    y_coord: YCoord,
    root: &DrawingArea<DB, Shift>,
) where
    DB: DrawingBackend,
{
    match x_coord {
        XCoord::Date => plot_results_impl(results, XIsDate, y_coord, root),
        XCoord::Index => plot_results_impl(results, XIsIndex, y_coord, root),
    }
}

// TODO: maybe plot by commit number?
fn plot_results_impl<DB, X>(
    results: &[CommitResult],
    x_coord: X,
    y_coord: YCoord,
    root: &DrawingArea<DB, Shift>,
) where
    DB: DrawingBackend,
    X: XCoordTrait,
{
    drop(x_coord);

    let x_values = results.iter().map(|c| X::get_x_coord(c));
    let min_x = x_values.clone().min().unwrap();
    let max_x = x_values.max().unwrap();

    let x_ranged = X::make_ranged(min_x, max_x);

    let max_count = results
        .iter()
        .map(|c| {
            let c = y_coord.get_counts(c);
            std::cmp::max(c.unsafe_, c.safe)
        })
        .max()
        .unwrap();

    root.fill(&WHITE).unwrap();
    let mut chart = ChartBuilder::on(&root)
        // .caption("y=x^2", ("sans-serif", 50).into_font())
        // .margin(5)
        .x_label_area_size(60)
        .y_label_area_size(60)
        .build_cartesian_2d(x_ranged, 0..max_count)
        .unwrap();

    chart
        .configure_mesh()
        .x_desc(X::AXIS_DESCRIPTION)
        .y_desc(match y_coord {
            YCoord::Functions => "Function count",
            YCoord::Expressions => "Expression count",
        })
        .axis_desc_style(("sans-serif", 15))
        .draw()
        .unwrap();

    let unsafe_series = LineSeries::new(
        results
            .iter()
            .map(|c| (X::get_x_coord(c), y_coord.get_counts(&c).unsafe_)),
        &RED,
    );
    let safe_series = LineSeries::new(
        results
            .iter()
            .map(|c| (X::get_x_coord(c), y_coord.get_counts(&c).safe)),
        &GREEN,
    );

    chart
        .draw_series(unsafe_series)
        .unwrap()
        .label("unsafe")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &RED));
    chart
        .draw_series(safe_series)
        .unwrap()
        .label("safe")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &GREEN));

    chart
        .configure_series_labels()
        .background_style(&WHITE.mix(0.8))
        .border_style(&BLACK)
        .draw()
        .unwrap();

    root.present().unwrap();
}
