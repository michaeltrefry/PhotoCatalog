import copy
import unittest
import edit_statistics as stats


def request(phase='kernel'):
    warmups, repeats = stats.COUNTS[phase]
    return dict(phase=phase, warmups=warmups, repetitions=repeats, recipes=[{'value': 1}],
                outputs=[], fixture_id='fixture', operation='test', source_sha256='a'*64,
                width=1600, height=1000)


def samples(r):
    return [dict(kind='observation', recipe_index=recipe, iteration=iteration,
                 warmup=iteration<r['warmups'], elapsed_ms=10000 if iteration<r['warmups'] else iteration-r['warmups']+1)
            for recipe, iteration in sorted(stats.expected_identities(r))]


class FixedTimingContracts(unittest.TestCase):
    def test_nearest_rank_and_warmups_do_not_hide_tail(self):
        r=request(); values=samples(r)
        result=stats.summarize_case(r,values)
        d=result['configurations'][0]['elapsed']
        self.assertEqual((d['count'],d['p50_ms'],d['p95_ms'],d['p99_ms'],d['max_ms']), (100,50,95,99,100))
        self.assertTrue(result['configurations'][0]['numeric_target_met'])
        self.assertFalse(result['whole_story_qualified'])
        self.assertTrue(result['independent_case_and_process_verification_required'])
        # Five slow samples affect p99/max but not the predeclared nearest-rank95.
        for row in values[-5:]:row['elapsed_ms']=500
        self.assertEqual(stats.summarize_case(r,values)['configurations'][0]['elapsed']['p95_ms'],95)
        values[-6]['elapsed_ms']=500
        self.assertFalse(stats.summarize_case(r,values)['configurations'][0]['numeric_target_met'])

    def test_nonfinite_negative_or_boolean_times_and_wrong_identities_rejected(self):
        r=request()
        for bad in (float('nan'),float('inf'),-1,True):
            values=samples(r); values[0]['elapsed_ms']=bad
            with self.assertRaises(ValueError):stats.summarize_case(r,values)
        for mutate in (lambda v:v.pop(),lambda v:v.append(v[0]),
                       lambda v:v[2].update(iteration=1),lambda v:v[2].update(recipe_index=999),
                       lambda v:v[2].update(iteration=True),lambda v:v[2].update(warmup=1)):
            values=samples(r);mutate(values)
            with self.assertRaises(ValueError):stats.summarize_case(r,values)
        changed=request();changed['repetitions']=99
        with self.assertRaises(ValueError):stats.summarize_case(changed,samples(r))

    def test_cameras_and_recipes_are_not_pooled_or_awarded_support_timing(self):
        r=request();r['recipes'].append({'value':2})
        values=samples(r)
        for row in values:
            if row['recipe_index']==1:row['elapsed_ms']=200
        result=stats.summarize_case(r,values)
        self.assertEqual([c['numeric_target_met'] for c in result['configurations']],[True,False])
        r=request('support100mp');r['width']=10000;r['height']=10000
        self.assertIsNone(stats.summarize_case(r,samples(r))['configurations'][0]['numeric_target_met'])

    def test_export_phase_times_are_actual_and_missing_intervals_rejected(self):
        r=request('export');r['outputs']=[{'format':{'format':'png'}}]
        values=samples(r)
        details=dict(worker_elapsed_ms=1,seal_ms=2,accept_ms=3,publication_elapsed_ms=4,
                     render={name:5 for name in stats.RENDER_FIELDS},
                     publication=dict(original_hash_ms=6,total_ms=7,
                        publication=dict(hash_ms=8,capture_ms=9,link_ms=10,durability_ms=11),
                        authority_intervals_ms=[12,13,14,15]))
        for row in values:row['phases']=copy.deepcopy(details)
        result=stats.summarize_case(r,values)['configurations'][0]
        self.assertEqual(result['p95_target_ms'],40000)
        self.assertEqual(result['phases']['render.decode_ms']['p95_ms'],5)
        self.assertEqual(result['phases']['publication.authority_3_ms']['max_ms'],15)
        values[0]['phases']['publication']['authority_intervals_ms'].pop()
        with self.assertRaises(ValueError):stats.summarize_case(r,values)

    def test_refusal_has_no_fake_timing_distribution(self):
        r=request('refusal');value=samples(r)[0];del value['elapsed_ms']
        self.assertEqual(stats.summarize_case(r,[value])['configurations'],[])


if __name__=='__main__':unittest.main()
