//! Human-readable Dutch descriptions and class guidance for contractor recognition.
//!
//! Amount thresholds reflect the FPS Economy's *Repartition des classes des
//! entrepreneurs agrees* reference, version 18 December 2025:
//! <https://economie.fgov.be/sites/default/files/Files/Entreprises/Repartition-classes-des-entrepreneurs-agrees.pdf>.
//! The Council of State annulled the 14 April 2024 threshold increase by judgment
//! 265.188 of 12 December 2025; this dated table shows the restored limits. Recheck
//! the official source before relying on these time-sensitive amounts. This lookup
//! is not a substitute for the complete legal rules governing recognition or eligibility.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RecognitionCategory {
    pub code: &'static str,
    pub description_nl: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClassAmountGuidance {
    pub class: u8,
    /// Maximum contract amount for classes 1–7, inclusive as stated in the source.
    pub maximum_amount_eur: Option<u64>,
    /// Class 8 applies above this amount, as stated in the source.
    pub above_amount_eur: Option<u64>,
}

pub const CATEGORIES: &[RecognitionCategory] = &[
    RecognitionCategory { code: "A", description_nl: "Algemene aannemingen van baggerwerken" },
    RecognitionCategory { code: "A1", description_nl: "Lichten van schepen en opruimen van wrakken" },
    RecognitionCategory { code: "B", description_nl: "Algemene aannemingen van waterbouwkundige werken" },
    RecognitionCategory { code: "B1", description_nl: "Ruimen van waterlopen" },
    RecognitionCategory { code: "C", description_nl: "Algemene aannemingen van wegenbouwkundige werken" },
    RecognitionCategory { code: "C1", description_nl: "Gewone rioleringswerken" },
    RecognitionCategory { code: "C2", description_nl: "Watervoorziening en leggen allerhande leidingen" },
    RecognitionCategory { code: "C3", description_nl: "Niet-elektrische verkeerstekens langs verbindingswegen, allerhande niet-elektrische veiligheidsinrichtingen, afsluitingen en schermen" },
    RecognitionCategory { code: "C5", description_nl: "Bitumineuze verhardingen en bestrijkingen" },
    RecognitionCategory { code: "C6", description_nl: "Leggen van sterkstroom-, en telecommunicatiekabels in sleuven, zonder aaneenkoppeling" },
    RecognitionCategory { code: "C7", description_nl: "Horizontale doorpersingen van buizen voor kabels en leidingen" },
    RecognitionCategory { code: "D", description_nl: "Algemene aannemingen van bouwwerken" },
    RecognitionCategory { code: "D1", description_nl: "Alle ruwbouwwerken en onder kap brengen van gebouwen" },
    RecognitionCategory { code: "D4", description_nl: "Geluids- en warmte-isolatie, lichte scheidingswanden, valse plafonds en valse vloeren, al dan niet geprefabriceerd" },
    RecognitionCategory { code: "D5", description_nl: "Schrijnwerk in het algemeen, houten spanten en trappen" },
    RecognitionCategory { code: "D6", description_nl: "Marmer- en steenhouwerswerk" },
    RecognitionCategory { code: "D7", description_nl: "Smeedwerk" },
    RecognitionCategory { code: "D8", description_nl: "Dakbedekkingen in asfalt- of gelijkaardige producten, dichtingswerken" },
    RecognitionCategory { code: "D10", description_nl: "Tegelwerk" },
    RecognitionCategory { code: "D11", description_nl: "Pleister- en raapwerk" },
    RecognitionCategory { code: "D12", description_nl: "Niet-metalen en niet-asfaltbedekkingen" },
    RecognitionCategory { code: "D13", description_nl: "Verfwerk" },
    RecognitionCategory { code: "D14", description_nl: "Glazenmakerswerk" },
    RecognitionCategory { code: "D15", description_nl: "Parketwerk" },
    RecognitionCategory { code: "D16", description_nl: "Sanitaire installaties en verwarmingsinstallaties met gas door middel van individuele toestellen" },
    RecognitionCategory { code: "D17", description_nl: "Centrale verwarming, thermische installaties" },
    RecognitionCategory { code: "D18", description_nl: "Ventilatie, luchtverwarming en airconditioning" },
    RecognitionCategory { code: "D20", description_nl: "Metalen schrijnwerk" },
    RecognitionCategory { code: "D21", description_nl: "Reinigen en opknappen van gevels" },
    RecognitionCategory { code: "D22", description_nl: "Metalen dakbedekking en zinkwerk" },
    RecognitionCategory { code: "D23", description_nl: "Restauratie door ambachtslieden" },
    RecognitionCategory { code: "D24", description_nl: "Restauratie van monumenten" },
    RecognitionCategory { code: "D25", description_nl: "Muurbekledingen en vloerbedekkingen met uitzondering van marmer-, parket- en tegelwerk" },
    RecognitionCategory { code: "D29", description_nl: "Vloerdeklaag en bekleding van industriële vloeren" },
    RecognitionCategory { code: "E", description_nl: "Algemene aannemingen van burgerlijke bouwkunde" },
    RecognitionCategory { code: "E1", description_nl: "Moerriolen" },
    RecognitionCategory { code: "E2", description_nl: "Paalfunderingen, dam- en diepwanden" },
    RecognitionCategory { code: "E4", description_nl: "Horizontale doorpersingen van samenstellende elementen van kunstwerken" },
    RecognitionCategory { code: "F", description_nl: "Algemene aannemingen van metaalconstructies" },
    RecognitionCategory { code: "F1", description_nl: "Montage- en demontagewerken (zonder leveringen)" },
    RecognitionCategory { code: "F2", description_nl: "Bouw van metalen draagstructuren" },
    RecognitionCategory { code: "F3", description_nl: "Industrieel schilderwerk" },
    RecognitionCategory { code: "G", description_nl: "Algemene aannemingen van grondwerken" },
    RecognitionCategory { code: "G1", description_nl: "Borings- en sonderingswerken en injecties" },
    RecognitionCategory { code: "G2", description_nl: "Draineerwerken" },
    RecognitionCategory { code: "G3", description_nl: "Beplantingen" },
    RecognitionCategory { code: "G4", description_nl: "Speciale bekledingen voor sportvelden" },
    RecognitionCategory { code: "G5", description_nl: "Afbraakwerken" },
    RecognitionCategory { code: "H", description_nl: "Algemene aannemingen van spoorwegen" },
    RecognitionCategory { code: "H1", description_nl: "Lassen van spoorstaven" },
    RecognitionCategory { code: "H2", description_nl: "Plaatsen van stroomdraden" },
    RecognitionCategory { code: "K", description_nl: "Algemene aannemingen van mechanische uitrustingen" },
    RecognitionCategory { code: "K1", description_nl: "Uitrustingen van kunstwerken en van industriële mechanica, zoals het mechanisch gedeelte van sluizen, hefbruggen, silo's of opslaginstallaties" },
    RecognitionCategory { code: "K2", description_nl: "Installaties van overladings- of hijstoestellen (kranen en rolbruggen...)" },
    RecognitionCategory { code: "K3", description_nl: "Oleomechanische uitrustingen" },
    RecognitionCategory { code: "L", description_nl: "Algemene aannemingen van hydromechanische uitrustingsinstallaties" },
    RecognitionCategory { code: "L1", description_nl: "Installaties van leidingen" },
    RecognitionCategory { code: "L2", description_nl: "Uitrustingen van pomp- en turbinestations" },
    RecognitionCategory { code: "M", description_nl: "Algemene aannemingen van elektronische uitrustingen" },
    RecognitionCategory { code: "M1", description_nl: "Elektronische uitrustingen met industriële of hoge frequentie met inbegrip van de uitrusting van voedingsstations" },
    RecognitionCategory { code: "N", description_nl: "Algemene aannemingen van transportinstallaties in gebouwen" },
    RecognitionCategory { code: "N1", description_nl: "Liften, goederenliften, roltrappen en roltapijten" },
    RecognitionCategory { code: "N2", description_nl: "Vervoer langs kokers of buizen van voorwerpen, documenten of goederen (pneumatisch, mechanisch) Hierin zijn ook begrepen het transport bij middel van karretjes, roltapijten, ononderbroken ketens" },
    RecognitionCategory { code: "P1", description_nl: "Elektrische installaties in gebouwen, inbegrepen de installaties van stroomaggregaten, de uitrustingen voor brand- en diefstalmelding, telecommunicaties in gebouwen en hun omgeving en installaties of uitrustingen van gemengde telefonie" },
    RecognitionCategory { code: "P2", description_nl: "Elektrische en elektromechanische installaties van kunstwerken en nijverheidsinrichtingen en elektrische buiteninstallaties" },
    RecognitionCategory { code: "P3", description_nl: "Installaties van bovengrondse elektriciteitsleidingen" },
    RecognitionCategory { code: "P4", description_nl: "Elektrische installaties van haveninrichtingen" },
    RecognitionCategory { code: "S", description_nl: "Algemene aannemingen van telecommunicatie-uitrustingen en van databeheer" },
    RecognitionCategory { code: "S1", description_nl: "Openbare telefoon- en telegraafuitrustingen" },
    RecognitionCategory { code: "S2", description_nl: "Uitrustingen voor afstandsbediening, afstandscontrole en afstandsmeting" },
    RecognitionCategory { code: "S3", description_nl: "Uitrustingen voor radio- en televisie-uitzendingen, radar- en antenne-installaties" },
    RecognitionCategory { code: "S4", description_nl: "Uitrustingen voor informatieverwerking en procesregeling" },
    RecognitionCategory { code: "T2", description_nl: "Bliksemafleiders, ontvangstantennes" },
    RecognitionCategory { code: "T3", description_nl: "Koelinrichtingen" },
    RecognitionCategory { code: "T4", description_nl: "Uitrustingen voor wasserijen en grote keukens" },
    RecognitionCategory { code: "T6", description_nl: "Slachthuisinrichtingen" },
    RecognitionCategory { code: "U", description_nl: "Installaties voor huisvuilverwerking" },
    RecognitionCategory { code: "V", description_nl: "Installaties voor waterzuivering" },
];

pub const CLASS_AMOUNT_GUIDANCE: &[ClassAmountGuidance] = &[
    ClassAmountGuidance {
        class: 1,
        maximum_amount_eur: Some(135_000),
        above_amount_eur: None,
    },
    ClassAmountGuidance {
        class: 2,
        maximum_amount_eur: Some(275_000),
        above_amount_eur: None,
    },
    ClassAmountGuidance {
        class: 3,
        maximum_amount_eur: Some(500_000),
        above_amount_eur: None,
    },
    ClassAmountGuidance {
        class: 4,
        maximum_amount_eur: Some(900_000),
        above_amount_eur: None,
    },
    ClassAmountGuidance {
        class: 5,
        maximum_amount_eur: Some(1_810_000),
        above_amount_eur: None,
    },
    ClassAmountGuidance {
        class: 6,
        maximum_amount_eur: Some(3_225_000),
        above_amount_eur: None,
    },
    ClassAmountGuidance {
        class: 7,
        maximum_amount_eur: Some(5_330_000),
        above_amount_eur: None,
    },
    ClassAmountGuidance {
        class: 8,
        maximum_amount_eur: None,
        above_amount_eur: Some(5_330_000),
    },
];

pub fn category(code: &str) -> Option<&'static RecognitionCategory> {
    let normalized = code.trim().to_ascii_uppercase();
    CATEGORIES.iter().find(|item| item.code == normalized)
}

pub fn class_amount_guidance(class: u8) -> Option<&'static ClassAmountGuidance> {
    CLASS_AMOUNT_GUIDANCE
        .iter()
        .find(|item| item.class == class)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supplied_category_codes_have_dutch_descriptions() {
        assert_eq!(
            category("d17").unwrap().description_nl,
            "Centrale verwarming, thermische installaties"
        );
        assert_eq!(
            category("C1").unwrap().description_nl,
            "Gewone rioleringswerken"
        );
        assert_eq!(category("P4").unwrap().code, "P4");
        assert!(category("D19").is_none());
        assert!(category("I1").is_none());
    }

    #[test]
    fn dated_fps_reference_class_five_and_six_amounts_are_preserved() {
        assert_eq!(
            class_amount_guidance(5).unwrap().maximum_amount_eur,
            Some(1_810_000)
        );
        assert_eq!(
            class_amount_guidance(6).unwrap().maximum_amount_eur,
            Some(3_225_000)
        );
    }

    #[test]
    fn class_amount_guidance_preserves_the_class_eight_open_top() {
        assert_eq!(
            class_amount_guidance(7).unwrap().maximum_amount_eur,
            Some(5_330_000)
        );
        assert_eq!(class_amount_guidance(8).unwrap().maximum_amount_eur, None);
        assert_eq!(
            class_amount_guidance(8).unwrap().above_amount_eur,
            Some(5_330_000)
        );
        assert!(class_amount_guidance(9).is_none());
    }
}
