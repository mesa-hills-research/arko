import unittest
from datetime import date

from tally.models import Transaction
from tally.rules import UNCATEGORIZED, Rule, categorize, load_rules


class RulesTest(unittest.TestCase):
    def setUp(self):
        self.rules = load_rules({"FRESH MARKET": "groceries", "CAFE": "dining", "METRO": "transport"})

    def spend(self, payee: str, category=None) -> Transaction:
        return Transaction(date(2026, 9, 1), payee, -500, category)

    def test_a_rule_matches_part_of_the_payee_in_any_case(self):
        self.assertTrue(Rule("cafe", "dining").matches("Cafe Luna"))
        self.assertEqual(categorize(self.spend("FRESH MARKET #12"), self.rules), "groceries")
        self.assertEqual(categorize(self.spend("metro transit"), self.rules), "transport")

    def test_a_category_set_by_hand_wins(self):
        self.assertEqual(categorize(self.spend("CAFE LUNA", "work"), self.rules), "work")

    def test_payees_without_a_rule_are_uncategorized(self):
        self.assertEqual(categorize(self.spend("HARDWARE DEPOT"), self.rules), UNCATEGORIZED)

    def test_rules_keep_their_order(self):
        self.assertEqual([rule.pattern for rule in self.rules], ["FRESH MARKET", "CAFE", "METRO"])


if __name__ == "__main__":
    unittest.main()
