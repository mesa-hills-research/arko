import unittest

from tally.money import format_amount, parse_amount, split_evenly


class ParseAmountTest(unittest.TestCase):
    def test_plain_and_negative_amounts(self):
        self.assertEqual(parse_amount("12.30"), 1230)
        self.assertEqual(parse_amount("-12.30"), -1230)
        self.assertEqual(parse_amount("7"), 700)

    def test_thousands_separators_and_symbols(self):
        self.assertEqual(parse_amount("1,234.56"), 123456)
        self.assertEqual(parse_amount("$5"), 500)
        self.assertEqual(parse_amount("€ 3.10"), 310)

    def test_parentheses_mean_negative(self):
        self.assertEqual(parse_amount("(12.30)"), -1230)

    def test_rounds_to_the_nearest_cent(self):
        self.assertEqual(parse_amount("0.125"), 12)
        self.assertEqual(parse_amount("0.135"), 14)

    def test_rejects_text(self):
        with self.assertRaises(ValueError):
            parse_amount("twelve")


class FormatAmountTest(unittest.TestCase):
    def test_formats_with_the_currency_symbol(self):
        self.assertEqual(format_amount(123456), "$1,234.56")
        self.assertEqual(format_amount(5, "EUR"), "€0.05")
        self.assertEqual(format_amount(-1230, "GBP"), "-£12.30")

    def test_unknown_currency_uses_its_code(self):
        self.assertEqual(format_amount(100, "CHF"), "CHF 1.00")


class SplitEvenlyTest(unittest.TestCase):
    def test_shares_differ_by_at_most_a_cent(self):
        self.assertEqual(split_evenly(1000, 3), [334, 333, 333])
        self.assertEqual(sum(split_evenly(1001, 4)), 1001)

    def test_needs_at_least_one_part(self):
        with self.assertRaises(ValueError):
            split_evenly(100, 0)


if __name__ == "__main__":
    unittest.main()
