package tools

// bucketLabel returns the display label for a latency bucket upper bound.
func bucketLabel(upperMs int) string {
	if upperMs >= 1000 {
		return "slow"
	}
	return "fast"
}

// SummarizeBuckets folds bucket upper bounds into a count per label.
func SummarizeBuckets(uppers []int) map[string]int {
	out := map[string]int{}
	for _, u := range uppers {
		out[bucketLabel(u)]++
	}
	return out
}
