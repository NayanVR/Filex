//! Cardinality-based execution choice with a hard, incremental candidate budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    FilterFirst,
    TextFirst,
}
#[derive(Debug, Clone, Copy)]
pub struct Plan {
    pub order: Order,
    pub batch: usize,
    pub remaining: usize,
}
impl Plan {
    pub fn new(
        text_estimate: usize,
        filter_estimate: Option<usize>,
        limit: usize,
        budget: usize,
    ) -> Self {
        Self {
            order: if filter_estimate.is_some_and(|count| count < text_estimate) {
                Order::FilterFirst
            } else {
                Order::TextFirst
            },
            batch: limit.min(budget),
            remaining: budget,
        }
    }
    pub fn next_batch(&mut self) -> usize {
        let next = self.batch.min(self.remaining);
        self.remaining -= next;
        self.batch = self.batch.saturating_mul(2);
        next
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selects_smaller_side_and_never_exceeds_budget() {
        let mut plan = Plan::new(80000, Some(4000), 100, 550);
        assert_eq!(plan.order, Order::FilterFirst);
        assert_eq!(
            [
                plan.next_batch(),
                plan.next_batch(),
                plan.next_batch(),
                plan.next_batch()
            ],
            [100, 200, 250, 0]
        );
        assert_eq!(Plan::new(3, Some(4000), 100, 1000).order, Order::TextFirst);
        assert_eq!(Plan::new(3, None, 100, 1000).order, Order::TextFirst);
    }
}
