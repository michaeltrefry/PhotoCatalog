import unittest
import edit_large_reference as large
import edit_reference as ref
import edit_qualification as qualification


class LargePointOracle(unittest.TestCase):
    @unittest.skipUnless(__import__('importlib.util',fromlist=['find_spec']).find_spec('numpy'),'scientific environment required')
    def test_point_evaluation_matches_independent_full_small_reference(self):
        np=ref.np_module()
        width,height=16,12
        source=np.array([[large.generated_source(x,y) for x in range(width)] for y in range(height)])
        for recipe in (qualification.recipe(),qualification.recipes()['combined'][0]):
            expected=ref.render(ref.fixture_to_linear(source),recipe)
            (w,h),point=large.point_evaluator(width,height,recipe)
            actual=np.array([[point(x,y) for x in range(w)] for y in range(h)])
            self.assertLess(float(np.abs(actual-expected).max()),1e-12)
            proof=large.verify_large(expected,recipe,width,height)
            self.assertGreater(proof['components'],0)

    @unittest.skipUnless(__import__('importlib.util',fromlist=['find_spec']).find_spec('numpy'),'scientific environment required')
    def test_mutated_neutral_component_rejected(self):
        np=ref.np_module()
        source=np.array([[large.generated_source(x,y) for x in range(16)] for y in range(12)])
        expected=ref.fixture_to_linear(source)
        expected[2,7,1]+=.01
        with self.assertRaises(ValueError):large.verify_large(expected,qualification.recipe(),16,12)

if __name__=='__main__':unittest.main()
