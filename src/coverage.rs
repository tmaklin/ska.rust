//! Tools for estimating a count cutoff with FASTQ input.
//!
//! This module has a basic k-mer counter using a dictionary, and then uses
//! maximum likelihood with some basic numerical optimisation to fit a two-component
//! mixture of Poissons to determine a coverage model. This can be used to classify
//! a count cutoff with noisy data.
//!
//! [`CoverageHistogram`] is the main interface.

use core::panic;

use argmin::core::observers::{ObserverMode, SlogLogger};
use argmin::core::{
    CostFunction, Error, Executor, Gradient, State, TerminationReason::SolverConverged,
};
use argmin::solver::linesearch::condition::ArmijoCondition;
use argmin::solver::linesearch::BacktrackingLineSearch;
use argmin::solver::quasinewton::BFGS;

use libm::lgamma;

use hashbrown::HashMap;
extern crate needletail;
use needletail::{parse_fastx_file, parser::Format};

use crate::ska_dict::bit_encoding::UInt;
use crate::ska_dict::split_kmer::SplitKmer;
use crate::QualFilter;

const MAX_COUNT: usize = 1000;
const MIN_FREQ: u32 = 50;
const INIT_W0: f64 = 0.8f64;
const INIT_C: f64 = 20.0f64;

/// K-mer counts and a coverage model for a single sample, using a pair of FASTQ files as input
///
/// Call [`CoverageHistogram::new()`] to count k-mers, then [`CoverageHistogram::fit_histogram()`]
/// to fit the model and find a cutoff. [`CoverageHistogram::plot_hist()`] can be used to
/// extract a table of the output for plotting purposes.
pub struct CoverageHistogram<IntT> {
    /// K-mer size
    k: usize,
    /// Whether reverse complement split k-mers were used
    rc: bool,
    /// Dictionary of k-mers and their counts
    kmer_dict: HashMap<IntT, u32>,
    /// Count histogram
    counts: Vec<u32>,
    /// Estimated error weight
    w0: f64,
    /// Estimated coverage
    c: f64,
    /// Coverage cutoff
    cutoff: usize,
    /// Show logging
    verbose: bool,
    /// Has the fit been run
    fitted: bool,
}

impl<IntT> CoverageHistogram<IntT>
where
    IntT: for<'a> UInt<'a>,
{
    /// Count split k-mers from a pair of input FASTQ files.
    ///
    /// Parameters the same as for [`crate::ska_dict::SkaDict`]. `verbose` will
    /// also print to stderr on each iteration of the optiser.
    pub fn new(fastq1: &String, fastq2: &String, k: usize, rc: bool, verbose: bool) -> Self {
        if !(5..=63).contains(&k) || k % 2 == 0 {
            panic!("Invalid k-mer length");
        }

        let mut cov_counts = Self {
            k,
            rc,
            kmer_dict: HashMap::default(),
            counts: vec![0; MAX_COUNT],
            w0: INIT_W0,
            c: INIT_C,
            cutoff: 0,
            verbose,
            fitted: false,
        };

        // Check if we're working with reads first
        for fastx_file in [fastq1, fastq2] {
            let mut reader_peek = parse_fastx_file(fastx_file)
                .unwrap_or_else(|_| panic!("Invalid path/file: {}", fastx_file));
            let seq_peek = reader_peek
                .next()
                .expect("Invalid FASTA/Q record")
                .expect("Invalid FASTA/Q record");
            if seq_peek.format() != Format::Fastq {
                panic!("{fastx_file} appears to be FASTA.\nCoverage can only be used with FASTQ files, not FASTA.");
            }
        }

        log::info!("Counting k-mers");
        for fastx_file in [fastq1, fastq2] {
            let mut reader = parse_fastx_file(fastx_file)
                .unwrap_or_else(|_| panic!("Invalid path/file: {fastx_file}"));
            while let Some(record) = reader.next() {
                let seqrec = record.expect("Invalid FASTA/Q record");
                let kmer_opt = SplitKmer::new(
                    seqrec.seq(),
                    seqrec.num_bases(),
                    seqrec.qual(),
                    cov_counts.k,
                    cov_counts.rc,
                    0,
                    QualFilter::NoFilter,
                    false,
                );
                if let Some(mut kmer_it) = kmer_opt {
                    let (kmer, _base, _rc) = kmer_it.get_curr_kmer();
                    cov_counts
                        .kmer_dict
                        .entry(kmer)
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                    while let Some((kmer, _base, _rc)) = kmer_it.get_next_kmer() {
                        cov_counts
                            .kmer_dict
                            .entry(kmer)
                            .and_modify(|count| *count += 1)
                            .or_insert(1);
                    }
                }
            }
        }

        cov_counts
    }

    /// Fit the coverage model to the histogram of counts
    ///
    /// Returns the fitted cutoff if successful.
    ///
    /// # Errors
    /// - If the optimiser didn't finish (reached 100 iterations or another problem).
    /// - If the linesearch cannot be constructed (may be a bounds issue, or poor data).
    /// - If the optimiser is still running (this shouldn't happen).
    ///
    /// # Panics
    /// - If the fit has already been run
    pub fn fit_histogram(&mut self) -> Result<usize, Error> {
        if self.fitted {
            panic!("Model already fitted");
        }

        // Calculate k-mer histogram
        log::info!("Calculating k-mer histogram");
        for kmer_count in self.kmer_dict.values() {
            let kc = (*kmer_count - 1) as usize;
            if kc < MAX_COUNT {
                self.counts[kc] += 1;
            }
        }

        // Truncate count vec and covert to float
        let mut counts_f64: Vec<f64> = Vec::new();
        for hist_bin in &self.counts {
            if *hist_bin < MIN_FREQ {
                break;
            } else {
                counts_f64.push(*hist_bin as f64);
            }
        }
        let count_len = counts_f64.len();

        log::info!("Fitting Poisson mixture model using maximum likelihood");
        let mixture_fit = MixPoisson { counts: counts_f64 };
        let init_param: Vec<f64> = vec![self.w0, self.c];
        let init_hessian: Vec<Vec<f64>> = vec![vec![1.0, 0.0], vec![0.0, 1.0]];
        let linesearch = BacktrackingLineSearch::new(ArmijoCondition::new(0.0001f64)?);
        let solver = BFGS::new(linesearch);
        let mut exec = Executor::new(mixture_fit, solver).configure(|state| {
            state
                .param(init_param)
                .inv_hessian(init_hessian)
                .max_iters(100)
        });
        if self.verbose {
            exec = exec.add_observer(SlogLogger::term(), ObserverMode::Always);
        }
        let res = exec.run()?;

        // print diagnostics
        log::info!("{res}");
        if let Some(termination_reason) = res.state().get_termination_reason() {
            if *termination_reason == SolverConverged {
                // Best parameter vector
                let best = res.state().get_best_param().unwrap();
                self.w0 = best[0];
                self.c = best[1];

                // calculate the coverage cutoff
                self.cutoff = find_cutoff(best, count_len);
                self.fitted = true;
                Ok(self.cutoff)
            } else {
                Err(Error::msg(format!(
                    "Optimiser did not converge: {}",
                    termination_reason.text()
                )))
            }
        } else {
            Err(Error::msg("Optimiser did not finish running"))
        }
    }

    /// Prints the counts and model to stdout, for use in plotting.
    ///
    /// Creates a table with count, number of k-mers at that count, mixture
    /// density, and most likely component.
    /// Plot with the `plot_hist.py` helper script.
    pub fn plot_hist(&self) {
        if !self.fitted {
            panic!("Model has not yet been fitted");
        }

        log::info!("Calculating and printing count series");
        println!("Count\tK_mers\tMixture_density\tComponent");
        for (idx, count) in self.counts.iter().enumerate() {
            if *count < MIN_FREQ {
                break;
            }
            println!(
                "{}\t{}\t{:e}\t{}",
                idx + 1,
                *count,
                f64::exp(lse(
                    a(self.w0, idx as f64 + 1.0),
                    b(self.w0, self.c, idx as f64 + 1.0)
                )),
                if (idx + 1) < self.cutoff {
                    "Error"
                } else {
                    "Coverage"
                }
            )
        }
    }
}

struct MixPoisson {
    counts: Vec<f64>,
}

impl CostFunction for MixPoisson {
    /// Type of the parameter vector
    type Param = Vec<f64>;
    /// Type of the return value computed by the cost function
    type Output = f64;

    /// Apply the cost function to a parameter `p`
    fn cost(&self, p: &Self::Param) -> Result<Self::Output, Error> {
        Ok(-log_likelihood(p, &self.counts))
    }
}

impl Gradient for MixPoisson {
    /// Type of the parameter vector
    type Param = Vec<f64>;
    /// Type of the gradient
    type Gradient = Vec<f64>;

    /// Compute the gradient at parameter `p`.
    fn gradient(&self, p: &Self::Param) -> Result<Self::Gradient, Error> {
        // Compute gradient of 2D Rosenbrock function
        Ok(grad_ll(p, &self.counts).iter().map(|x| -*x).collect())
    }
}

// log-sum-exp
fn lse(a: f64, b: f64) -> f64 {
    let xstar = f64::max(a, b);
    xstar + f64::ln(f64::exp(a - xstar) + f64::exp(b - xstar))
}

// Natural log of Poisson density
fn ln_dpois(x: f64, lambda: f64) -> f64 {
    x * f64::ln(lambda) - lgamma(x + 1.0) - lambda
}

// error component
fn a(w0: f64, i: f64) -> f64 {
    f64::ln(w0) + ln_dpois(i, 1.0)
}

// coverage component
fn b(w0: f64, c: f64, i: f64) -> f64 {
    f64::ln(1.0 - w0) + ln_dpois(i, c)
}

// Mixture likelihood
fn log_likelihood(pars: &[f64], counts: &[f64]) -> f64 {
    let w0 = pars[0];
    let c = pars[1];
    let mut ll = 0.0;
    if !(0.0..=1.0).contains(&w0) || c < 1.0 {
        ll = f64::MIN;
    } else {
        for (i, count) in counts.iter().enumerate() {
            let i_f64 = i as f64 + 1.0;
            ll += *count * lse(a(w0, i_f64), b(w0, c, i_f64));
        }
    }
    ll
}

fn grad_ll(pars: &[f64], counts: &[f64]) -> Vec<f64> {
    let w0 = pars[0];
    let c = pars[1];

    let mut grad_w0 = 0.0;
    let mut grad_c = 0.0;
    for (i, count) in counts.iter().enumerate() {
        let i_f64 = i as f64 + 1.0;
        let a_val = a(w0, i_f64);
        let b_val = b(w0, c, i_f64);
        let dlda = 1.0 / (1.0 + f64::exp(b_val - a_val));
        let dldb = 1.0 / (1.0 + f64::exp(a_val - b_val));
        grad_w0 += *count * (dlda / w0 - dldb / (1.0 - w0));
        grad_c += *count * (dldb * (i_f64 / c - 1.0));
    }
    vec![grad_w0, grad_c]
}

fn find_cutoff(pars: &[f64], max_cutoff: usize) -> usize {
    let w0 = pars[0];
    let c = pars[1];

    let mut cutoff = 1;
    while cutoff < max_cutoff {
        let cutoff_f64 = cutoff as f64;
        let root = a(w0, cutoff_f64) - b(w0, c, cutoff_f64);
        if root < 0.0 {
            break;
        }
        cutoff += 1;
    }
    cutoff
}
