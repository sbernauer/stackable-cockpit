use std::{collections::HashMap, marker::PhantomData};

use serde::de::DeserializeOwned;
use snafu::ResultExt;

use crate::{
    utils::templating,
    xfer::{Result, TemplatingSnafu, YamlSnafu},
};

pub trait Processor: Sized {
    type Input;
    type Output;

    /// Processes the input and returns the altered content.
    fn process(&self, input: Self::Input) -> Result<Self::Output>;

    /// Chains this processors output as the input of the `other` processor.
    fn then<P: Processor<Input = Self::Output>>(self, other: P) -> Chain<Self, P> {
        Chain(self, other)
    }
}

#[derive(Debug)]
pub struct Chain<P1, P2>(P1, P2);

impl<P1, P2> Processor for Chain<P1, P2>
where
    P1: Processor,
    P2: Processor<Input = P1::Output>,
{
    type Input = P1::Input;
    type Output = P2::Output;

    fn process(&self, input: Self::Input) -> Result<Self::Output> {
        self.1.process(self.0.process(input)?)
    }
}

/// Process plain text without any encodings like YAML or JSON and without any
/// templating applied to it.
#[derive(Debug, Default)]
pub struct Text;

impl Processor for Text {
    type Input = String;
    type Output = String;

    fn process(&self, input: Self::Input) -> Result<Self::Output> {
        Ok(input)
    }
}

/// Process the contents by parsing it as YAML.
#[derive(Debug)]
pub struct Yaml<T>(PhantomData<T>);

impl<T> Processor for Yaml<T>
where
    T: DeserializeOwned,
{
    type Input = String;
    type Output = T;

    fn process(&self, input: Self::Input) -> Result<Self::Output> {
        serde_yaml::from_str(&input).context(YamlSnafu)
    }
}

impl<T> Default for Yaml<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T> Yaml<T> {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Process the contents by rendering templated parts of the contents.
#[derive(Debug)]
pub struct Template<'a>(&'a HashMap<String, String>);

impl<'a> Processor for Template<'a> {
    type Input = String;
    type Output = String;

    fn process(&self, input: Self::Input) -> Result<Self::Output> {
        templating::render(&input, self.0).context(TemplatingSnafu)
    }
}

impl<'a> Template<'a> {
    pub fn new(parameters: &'a HashMap<String, String>) -> Self {
        Self(parameters)
    }
}
