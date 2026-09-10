import unittest
import xml.etree.ElementTree as ET
import edit_correctness_matrix as matrix
import edit_derivative as derivative
import edit_qualification as qualification


class DerivativeExpectations(unittest.TestCase):
    def test_unselected_source_still_has_regenerated_technical_metadata(self):
        for selected in ({},{'xmp':None}):
            with self.subTest(selected=selected):
                before=selected.copy()
                expected=derivative.expected_metadata(selected,48,32,qualification.outputs()['jpeg8'])
                root=ET.fromstring(expected['xmp'])
                description=root.find('.//{'+derivative.RDF+'}Description')
                self.assertEqual(description.get('{'+derivative.RDF+'}about'),'')
                self.assertEqual(root.find('.//{'+derivative.TIFF+'}ImageWidth').text,'48')
                self.assertEqual(root.find('.//{'+derivative.EXIF+'}PixelYDimension').text,'32')
                self.assertEqual(root.find('.//{'+derivative.XMP+'}CreatorTool').text,'PhotoCatalog')
                self.assertEqual(root.find('.//{'+derivative.DC+'}format').text,'image/jpeg')
                self.assertEqual(len(list(description)),12)
                self.assertNotIn('preserve this user text',expected['xmp'])
                self.assertEqual(selected,before)

    def test_preserves_subject_qualifiers_unknown_arrays_and_user_comment(self):
        selected=matrix.metadata(True)
        expected=derivative.expected_metadata(selected,48,32,qualification.outputs()['png16'])
        root=ET.fromstring(expected['xmp'])
        description=root.find('.//{'+derivative.RDF+'}Description')
        self.assertEqual(description.get('{'+derivative.RDF+'}about'),'urn:photocatalog:qualification:subject')
        self.assertEqual(root.find('.//{'+derivative.EXIF+'}UserComment').text,'preserve this user text')
        self.assertIsNotNone(root.find('.//{https://photocatalog.invalid/qualification/1/}structure'))
        self.assertEqual(len(root.findall('.//{'+derivative.RDF+'}Seq')),2)
        self.assertIsNone(root.find('.//{'+derivative.CRS+'}Exposure2012'))
        self.assertIsNone(root.find('.//{'+derivative.TIFF+'}StripOffsets'))
        self.assertIn('Exposure2012',selected['xmp'])

    def test_output_dimensions_channels_depth_are_derived_from_request(self):
        spec=qualification.outputs()['tiff32']
        spec['size']=dict(mode='fit',width=24,height=24,allow_upscale=False)
        spec['alpha']=dict(mode='composite',linear_rgb=[0,0,0])
        expected=derivative.expected_metadata(matrix.metadata(),48,32,spec)
        root=ET.fromstring(expected['xmp'])
        for field,value in (('ImageWidth','24'),('ImageLength','16'),('SamplesPerPixel','3'),('Orientation','1')):
            self.assertEqual(root.find('.//{'+derivative.TIFF+'}'+field).text,value)
        depth=root.find('.//{'+derivative.TIFF+'}BitsPerSample')
        self.assertEqual([item.text for item in depth.iter('{'+derivative.RDF+'}li')],['32']*3)

if __name__=='__main__':unittest.main()
