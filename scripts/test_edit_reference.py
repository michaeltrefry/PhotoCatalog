"""Bounded reference contracts; scientific cases run in the qualification env."""
import math
import struct
import unittest
from unittest.mock import patch
import edit_reference as ref
import edit_readback as readback
import edit_qualification as q


class ScalarContracts(unittest.TestCase):
    def test_profile_exact_size_and_transfer_tags(self):
        for gamma in (1,2):
            data=ref.matrix_profile(gamma)
            self.assertEqual(int.from_bytes(data[:4],'big'),len(data))
            self.assertEqual(data[36:40],b'acsp')
            count=int.from_bytes(data[128:132],'big')
            tags={data[132+12*i:136+12*i]:struct.unpack_from('>II',data,136+12*i) for i in range(count)}
            for channel in b'rgb':
                at,size=tags[bytes([channel])+b'TRC']
                self.assertEqual(data[at:at+4],b'curv')
                self.assertEqual(int.from_bytes(data[at+12:at+14],'big'),gamma*256)
                self.assertLessEqual(at+size,len(data))

    def test_robertson_white_point_has_valid_xy_and_tint_moves(self):
        for kelvin in (4300,6500):
            zero=ref.selected_xy(kelvin,0)
            tinted=ref.selected_xy(kelvin,5)
            self.assertTrue(all(0<v<1 for v in zero))
            self.assertLess(sum(zero),1)
            self.assertNotEqual(zero,tinted)
        with self.assertRaises(KeyError):
            ref.selected_xy(7000,0)

    def test_compressed_metadata_overflow_and_tail_rejected(self):
        import zlib
        payload=zlib.compress(b'x'*100)
        self.assertEqual(readback.inflate_limited(payload,100),b'x'*100)
        for data,limit in [(payload,99),(payload+b'tail',100),(payload[:-1],100)]:
            with self.assertRaises(ValueError):
                readback.inflate_limited(data,limit)

    def test_absent_or_invented_exif_pointer_rejected(self):
        self.assertEqual(readback.exif_tags(None),{})
        with self.assertRaises(ValueError):
            readback.exif_tags(b'II*\0'+(2**32-1).to_bytes(4,'little'))


class ScientificContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        try:
            cls.np=ref.np_module()
        except ImportError:
            raise unittest.SkipTest('qualification scientific environment not installed; this is not acceptance')

    def test_neutral_exposure_and_mask_are_analytic(self):
        np=self.np
        p=np.array([[[2,-.25,.5,.25],[.18,.18,.18,0]]],dtype=float)
        neutral=ref.render(p,q.recipe())
        self.assertTrue(np.allclose(neutral,p,atol=1e-14,rtol=0))
        adjusted=ref.render(p,q.recipe(exposure_ev=1))
        self.assertTrue(np.allclose(adjusted[...,:3],p[...,:3]*2,atol=1e-14,rtol=0))
        self.assertTrue(np.array_equal(adjusted[...,3],p[...,3]))
        self.assertFalse(ref.compare(adjusted,p)['pass_'])

    def test_constant_detail_and_alpha_preservation(self):
        np=self.np
        p=np.full((5,7,4),.18)
        p[...,3]=.25
        self.assertTrue(np.allclose(ref.sharpen(p,dict(amount=.5,radius_px=1.25)),p,atol=1e-14))
        self.assertTrue(np.allclose(ref.denoise(p,dict(luminance=.25,chroma=.5)),p,atol=1e-14))

    def test_premultiplied_geometry_does_not_leak_hidden_rgb(self):
        np=self.np
        p=np.array([[[10,0,0,0],[0,0,1,1]]],dtype=float)
        result=ref.resize(p,1,1)
        self.assertTrue(np.array_equal(result,np.array([[[0,0,1,.5]]])))

    def test_tolerance_rejects_channel_swap_and_wrong_alpha(self):
        np=self.np
        expected=np.array([[[.1,.2,2,.5]]])
        self.assertTrue(ref.compare(expected.copy(),expected)['pass_'])
        bad=expected.copy()
        bad[...,0]=2
        self.assertFalse(ref.compare(bad,expected)['pass_'])
        bad=expected.copy()
        bad[...,3]=1
        self.assertFalse(ref.compare(bad,expected)['pass_'])

if __name__=='__main__':
    unittest.main()
