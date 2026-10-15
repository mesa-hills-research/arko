import unittest
from datetime import date

from tally.dates import in_month, month_key, parse_date, parse_month, previous_month, week_start


class DatesTest(unittest.TestCase):
    def test_parses_the_usual_formats(self):
        self.assertEqual(parse_date("2026-09-04"), date(2026, 9, 4))
        self.assertEqual(parse_date("09/04/2026"), date(2026, 9, 4))
        self.assertEqual(parse_date("04.09.2026"), date(2026, 9, 4))
        with self.assertRaises(ValueError):
            parse_date("yesterday")

    def test_month_keys(self):
        self.assertEqual(month_key(date(2026, 3, 9)), "2026-03")
        self.assertEqual(parse_month("2026-03"), (2026, 3))
        with self.assertRaises(ValueError):
            parse_month("2026-13")
        with self.assertRaises(ValueError):
            parse_month("March")

    def test_in_month_covers_the_whole_month(self):
        self.assertTrue(in_month(date(2026, 9, 1), "2026-09"))
        self.assertTrue(in_month(date(2026, 9, 30), "2026-09"))
        self.assertFalse(in_month(date(2026, 10, 1), "2026-09"))
        self.assertTrue(in_month(date(2028, 2, 29), "2028-02"))

    def test_previous_month(self):
        self.assertEqual(previous_month("2026-09"), "2026-08")
        self.assertEqual(previous_month("2027-01"), "2026-12")

    def test_week_start(self):
        thursday = date(2026, 9, 10)
        self.assertEqual(week_start(thursday), date(2026, 9, 6))
        self.assertEqual(week_start(thursday, "monday"), date(2026, 9, 7))
        self.assertEqual(week_start(date(2026, 9, 6), "sunday"), date(2026, 9, 6))


if __name__ == "__main__":
    unittest.main()
